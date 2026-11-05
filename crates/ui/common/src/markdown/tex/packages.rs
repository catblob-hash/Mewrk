//! The TeX packages `lib/mathJaxEngine.ts` loads past `base` and `ams`, one section each, in the order MathJax asks them: a package
//! listed later in `TEX_PACKAGES` goes first, which is how `physics` comes to own `\sin` and `\div`. Most of it is what MathJax's own
//! methods are: text put back in the input. What is left over for `math_layout.rs` is a handful of `Node`s.

use super::*;
use std::collections::HashSet;
use std::sync::LazyLock;

/// Every name a loaded package defines in math, from the token maps of MathJax 4.1.3. What tells `\text{\alpha}`, which is an error,
/// from `\text{\nonsense}`, which is `noundefined`'s red.
pub(crate) const COMMANDS: &str = include_str!("commands.txt");

static IN_MATH: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    COMMANDS
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.split('\t').skip(1);
            let kind = parts.next()?;
            let name = parts.next()?;
            (kind == "C").then_some(name)
        })
        .collect()
});

pub(super) fn defined_in_math(name: &str) -> bool {
    IN_MATH.contains(name)
}

/// What a command of a package adds to how deep a formula counts as being.
pub(super) const DEEP_COMMAND: u32 = 2;

fn is_biggs(name: &str) -> bool {
    matches!(name, "big" | "Big" | "bigg" | "Bigg")
}

/// The char `\left` would read for a fence's own spelling.
fn fence_char(fence: &str) -> char {
    lookup_delimiter(fence).unwrap_or('\0')
}

impl Parser {
    /// A package's meaning for a name. Answers whether there was one.
    pub(super) fn package_command(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        // These hold a lot on the stack on the way to what they contain, so a level of them counts for more.
        self.depth += DEEP_COMMAND;
        if self.depth > MAX_DEPTH {
            self.depth -= DEEP_COMMAND;
            return Err(TOO_DEEP.to_owned());
        }
        let result = self.package_meaning(name, nodes);
        self.depth -= DEEP_COMMAND;
        result
    }

    fn package_meaning(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        Ok(self.units_and_unicode(name, nodes)?
            || self.textcomp(name, nodes)
            || self.physics(name, nodes)?
            || self.mhchem(name, nodes)?
            || self.mathtools(name, nodes)?
            || self.drawn(name, nodes)?
            || self.empheq_delimiter(name, nodes)?
            || self.colortbl(name)?
            || self.proof_command(name)?
            || self.braket(name, nodes)?
            || self.begingroup(name)?
            || self.base_extras(name, nodes)?)
    }

    fn parsed(&mut self, tex: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let parsed = self.parse_string(tex)?;
        nodes.push(Node::Group(parsed));
        Ok(true)
    }

    /// `GetUpTo`: raw text to a token at no depth of braces, and past it.
    fn get_up_to(&mut self, name: &str, token: char) -> Parsed<String> {
        self.skip_space();
        let start = self.at;
        let mut depth = 0;
        while let Some(c) = self.peek() {
            if depth == 0 && c == token {
                let raw = self.text[start..self.at].to_owned();
                self.bump();
                return Ok(raw);
            }
            self.bump();
            match c {
                '\\' => {
                    self.bump();
                }
                '{' => depth += 1,
                '}' => {
                    if depth == 0 {
                        return Err(format!("Extra close brace while looking for {token}"));
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        Err(format!("Couldn't find closing '{token}' for argument to \\{name}"))
    }

    fn next_is(&mut self, c: char) -> bool {
        self.skip_space();
        self.peek() == Some(c)
    }

    // ------------------------------------------------------------ newcommand and begingroup

    pub(super) fn lookup_macro(&self, name: &str) -> Option<Option<Macro>> {
        self.defs.scopes.iter().rev().find_map(|scope| scope.macros.get(name).cloned())
    }

    pub(super) fn lookup_env(&self, name: &str) -> Option<EnvDef> {
        self.defs.scopes.iter().rev().find_map(|scope| scope.envs.get(name).cloned())
    }

    /// In the group `begingroup` has open, or in the one that is always there. `\global` puts it past every group, up to a sandbox.
    fn scope_for_definition(&mut self) -> &mut Scope {
        if self.defs.scopes.is_empty() {
            self.defs.scopes.push(Scope::default());
        }
        let top = self.defs.scopes.len() - 1;
        let at = if std::mem::take(&mut self.global) { self.defs.scopes.iter().rposition(|s| s.sandbox).unwrap_or(0) } else { top };
        &mut self.defs.scopes[at]
    }

    fn define(&mut self, name: String, definition: Macro) {
        self.scope_for_definition().macros.insert(name, Some(definition));
    }

    pub(super) fn run_macro(&mut self, name: &str, definition: Macro, nodes: &mut Vec<Node>) -> Parsed<()> {
        match definition {
            Macro::Text { args, opt, body } => self.expand_with(name, args, opt.as_deref(), &body),
            Macro::Template { before, body } => {
                let values = self.template_args(name, &before)?;
                self.splice(&substitute(&body, &values))
            }
            Macro::Alias(target) => {
                self.expansions += 1;
                if self.expansions > MAX_EXPANSIONS {
                    return Err("MathJax maximum macro substitution count exceeded; is there a recursive macro call?".to_owned());
                }
                self.command(&target, nodes)
            }
            Macro::Paired { open, close, body, args, pre, post } => self.paired(name, &open, &close, &body, args, &pre, &post),
            Macro::Arrow { glyph, pad } => {
                let arrow = XArrow { left: false, right: false, head: ArrowHead::Glyph(glyph), pad };
                nodes.push(self.x_arrow_node(name, arrow)?);
                Ok(())
            }
            Macro::Aboxed { boxed, math } => self.aboxed(name, &boxed, math),
        }
    }

    /// `\def\a#1.#2;{...}`'s arguments.
    fn template_args(&mut self, name: &str, before: &[String]) -> Parsed<Vec<String>> {
        let mismatch = || format!("Use of \\{name} doesn't match its definition");
        if let Some(lead) = before.first()
            && !lead.is_empty()
        {
            if !self.text[self.at..].starts_with(lead.as_str()) {
                return Err(mismatch());
            }
            self.at += lead.len();
        }
        let mut values = Vec::new();
        for delimiter in &before[1.min(before.len())..] {
            if delimiter.is_empty() {
                values.push(self.get_arg(name)?);
                continue;
            }
            let start = self.at;
            let mut depth = 0;
            loop {
                if depth == 0 && self.text[self.at..].starts_with(delimiter.as_str()) {
                    values.push(self.text[start..self.at].to_owned());
                    self.at += delimiter.len();
                    break;
                }
                let Some(c) = self.bump() else { return Err(mismatch()) };
                match c {
                    '\\' => {
                        self.bump();
                    }
                    '{' => depth += 1,
                    '}' if depth == 0 => return Err(mismatch()),
                    '}' => depth -= 1,
                    _ => {}
                }
            }
        }
        Ok(values)
    }

    /// `GetCsNameArgument`: the backslash is optional, and the rest is one name.
    fn cs_name_argument(&mut self, name: &str) -> Parsed<String> {
        let raw = self.get_arg(name)?;
        let raw = raw.trim();
        let rest = raw.strip_prefix('\\').unwrap_or(raw);
        if rest.chars().count() != 1 && (rest.is_empty() || !rest.chars().all(is_letter)) {
            return Err(format!("Illegal control sequence name for \\{name}"));
        }
        Ok(rest.to_owned())
    }

    fn arg_count(&mut self, name: &str) -> Parsed<u8> {
        let Some(raw) = self.get_brackets(name)? else { return Ok(0) };
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(0);
        }
        let count: u8 = raw.parse().map_err(|_| format!("Illegal number of parameters specified in \\{name}"))?;
        if count > 9 {
            return Err(format!("Illegal number of parameters specified in \\{name}"));
        }
        Ok(count)
    }

    /// `\newcommand` and its kind. They stay defined for the formulas after this one, as they do in upstream's one input jax.
    pub(super) fn definitions(&mut self, name: &str) -> Parsed<Option<bool>> {
        match name {
            "newcommand" | "renewcommand" => {
                let target = self.cs_name_argument(name)?;
                let args = self.arg_count(name)?;
                let opt = self.get_brackets(name)?;
                let body = self.get_arg(name)?;
                self.define(target, Macro::Text { args, opt, body });
                Ok(Some(true))
            }
            "newenvironment" | "renewenvironment" => {
                let env = self.get_arg(name)?.trim().to_owned();
                let args = self.arg_count(name)?;
                let opt = self.get_brackets(name)?;
                let begin = self.get_arg(name)?;
                let end = self.get_arg(name)?;
                self.scope_for_definition().envs.insert(env, EnvDef { args, opt, begin, end });
                Ok(Some(true))
            }
            "def" | "gdef" => {
                self.global |= name == "gdef";
                self.skip_space();
                if self.bump() != Some('\\') {
                    return Err(format!("\\{name} must be followed by a control sequence"));
                }
                let target = self.control_name();
                let start = self.at;
                while self.peek().is_some_and(|c| c != '{') {
                    self.bump();
                }
                let template = self.text[start..self.at].to_owned();
                let body = self.get_arg(name)?;
                let definition = def_template(&template, body).ok_or_else(|| format!("Illegal parameter number in definition of \\{target}"))?;
                self.define(target, definition);
                Ok(Some(true))
            }
            "let" => {
                self.skip_space();
                if self.bump() != Some('\\') {
                    return Err("\\let must be followed by a control sequence".to_owned());
                }
                let target = self.control_name();
                self.skip_space();
                self.eat('=');
                self.skip_space();
                match self.bump() {
                    Some('\\') => {
                        let other = self.control_name();
                        if other != target {
                            // Taking a name's meaning as it stands now, whatever that later becomes.
                            let definition = match self.lookup_macro(&other) {
                                Some(Some(definition)) => definition,
                                Some(None) => {
                                    self.scope_for_definition().macros.insert(target, None);
                                    return Ok(Some(true));
                                }
                                None => Macro::Alias(other),
                            };
                            self.define(target, definition);
                        }
                    }
                    Some(c) => self.define(target, Macro::Text { args: 0, opt: None, body: c.to_string() }),
                    None => return Err("Missing argument for \\let".to_owned()),
                }
                Ok(Some(true))
            }
            "DeclareMathOperator" => {
                let limits = self.star();
                let target = self.cs_name_argument(name)?;
                let body = self.get_arg(name)?;
                let star = if limits { "*" } else { "" };
                self.define(target, Macro::Text { args: 0, opt: None, body: format!("\\operatorname{star}{{{body}}}") });
                Ok(Some(true))
            }
            "newcolumntype" => {
                let column = self.get_arg(name)?;
                let args = self.arg_count(name)?;
                let body = self.get_arg(name)?;
                if column.chars().count() != 1 {
                    return Err(format!("Column specifier must be exactly one character: {column}"));
                }
                self.defs.columns.insert(column, (args, body));
                Ok(Some(true))
            }
            _ => Ok(None),
        }
    }

    /// What a `\newenvironment` opens: its two texts around whatever stands before its `\end`, in a group.
    pub(super) fn defined_environment(&mut self, name: &str, definition: EnvDef) -> Parsed<Node> {
        let mut values = Vec::with_capacity(definition.args as usize);
        if definition.args > 0
            && let Some(default) = &definition.opt
        {
            values.push(self.get_brackets(name)?.unwrap_or_else(|| default.clone()));
        }
        while values.len() < definition.args as usize {
            values.push(self.get_arg(name)?);
        }
        let begin = substitute(&definition.begin, &values);
        let start = self.at;
        let (open, close) = (format!("\\begin{{{name}}}"), format!("\\end{{{name}}}"));
        let mut depth = 0;
        let body_end = loop {
            let rest = &self.text[self.at..];
            if rest.is_empty() {
                return Err(format!("Missing \\end{{{name}}}"));
            }
            if rest.starts_with(&open) {
                depth += 1;
                self.at += open.len();
            } else if rest.starts_with(&close) {
                if depth == 0 {
                    break self.at;
                }
                depth -= 1;
                self.at += close.len();
            } else {
                let escaped = rest.starts_with('\\');
                self.bump();
                if escaped {
                    self.bump();
                }
            }
        };
        let inner = format!("{begin} {} {}", &self.text[start..body_end], definition.end);
        self.at = body_end + close.len();
        self.expansions += 1;
        if self.expansions > MAX_EXPANSIONS {
            return Err("MathJax maximum macro substitution count exceeded; is there a recursive macro call?".to_owned());
        }
        Ok(Node::Group(self.parse_string(&inner)?))
    }

    fn begingroup(&mut self, name: &str) -> Parsed<bool> {
        match name {
            "begingroup" => self.defs.scopes.push(Scope::default()),
            "endgroup" => {
                if self.defs.scopes.len() <= 1 || self.defs.scopes.last().is_some_and(|s| s.sandbox) {
                    return Err("Extra \\endgroup or missing \\begingroup".to_owned());
                }
                self.defs.scopes.pop();
            }
            "global" => {
                self.skip_space();
                let rest = &self.text[self.at..];
                let next: String = rest.strip_prefix('\\').map_or(String::new(), |r| r.chars().take_while(|c| is_letter(*c)).collect());
                if !matches!(next.as_str(), "let" | "def" | "newcommand" | "DeclareMathOperator" | "Newextarrow") {
                    return Err("Invalid use of \\global".to_owned());
                }
                self.global = true;
            }
            "begingroupReset" => {
                let from = self.defs.scopes.iter().rposition(|s| s.sandbox).map_or(1, |at| at + 1);
                self.defs.scopes.truncate(from);
            }
            "begingroupSandbox" => self.defs.scopes.push(Scope { sandbox: true, ..Scope::default() }),
            _ => return Ok(false),
        }
        Ok(true)
    }

    // ------------------------------------------------------------ units, unicode, textcomp

    fn units_and_unicode(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        match name {
            "units" => {
                let value = self.get_brackets(name)?;
                let unit = self.get_arg(name)?;
                let lead = value.filter(|v| !v.is_empty()).map_or(String::new(), |v| format!("{v}\\,"));
                self.splice(&format!("{lead}\\mathrm{{{unit}}}"))?;
                Ok(true)
            }
            "unitfrac" => {
                let value = self.get_brackets(name)?;
                let num = self.get_arg(name)?;
                let den = self.get_arg(name)?;
                let lead = value.filter(|v| !v.is_empty()).map_or(String::new(), |v| format!("{v}\\,"));
                self.splice(&format!("{lead}\\nicefrac[\\mathrm]{{{num}}}{{{den}}}"))?;
                Ok(true)
            }
            "nicefrac" => {
                let font = self.get_brackets(name)?.unwrap_or_else(|| "\\mathrm".to_owned());
                let num = self.get_arg(name)?;
                let den = self.get_arg(name)?;
                let num = self.parse_string(&format!("{font}{{{num}}}"))?;
                let den = self.parse_string(&format!("{font}{{{den}}}"))?;
                nodes.push(Node::Frac(Box::new(Frac { num, den, bar: None, style: None, left: '\0', right: '\0', continued: false, align: (0, 0), bevelled: true })));
                Ok(true)
            }
            "U" => {
                let hex = self.get_arg(name)?;
                let hex = hex.trim();
                if hex.is_empty() || hex.len() > 6 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)) {
                    return Err(format!("Argument to \\{name} must a hexadecimal number with 1 to 6 digits"));
                }
                let c = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).ok_or_else(|| format!("Argument to \\{name} must a hexadecimal number with 1 to 6 digits"))?;
                self.splice(&c.to_string())?;
                Ok(true)
            }
            "char" => {
                self.skip_space();
                let c = self.char_number().ok_or_else(|| "Missing character number for \\char".to_owned())?;
                let c = char::from_u32(c).ok_or_else(|| "Invalid character number for \\char".to_owned())?;
                nodes.push(Node::Text { text: c.to_string(), font: MathFont::Roman });
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// `\char'101`, `\char"41`, `` \char`A ``, `\char65`.
    fn char_number(&mut self) -> Option<u32> {
        let take = |this: &mut Self, radix: u32, most: usize| {
            let start = this.at;
            let mut count = 0;
            while count < most && this.peek().is_some_and(|c| c.is_digit(radix)) {
                this.bump();
                count += 1;
            }
            let value = u32::from_str_radix(&this.text[start..this.at], radix).ok();
            if this.peek() == Some(' ') {
                this.bump();
            }
            value
        };
        match self.peek()? {
            '\'' => {
                self.bump();
                take(self, 8, 7)
            }
            '"' => {
                self.bump();
                take(self, 16, 6)
            }
            '`' => {
                self.bump();
                let c = self.bump()?;
                if c == '\\' {
                    self.bump().map(u32::from)
                } else {
                    Some(c as u32)
                }
            }
            c if c.is_ascii_digit() => take(self, 10, 7),
            _ => None,
        }
    }

    fn textcomp(&mut self, name: &str, nodes: &mut Vec<Node>) -> bool {
        let Some(text) = text_symbol(name) else { return false };
        nodes.push(Node::Text { text: text.to_owned(), font: MathFont::Roman });
        true
    }

    // ------------------------------------------------------------ physics

    /// `AutoOpen`: what is read from here to the closer, between the two of them. A brace or a `\left` in between has its own list, and
    /// what stands in there is not the closer.
    fn auto_open(&mut self, open: char, close: char, big: Option<&str>, smash: bool, phantom: bool, ignore: bool) -> Parsed<Option<Node>> {
        self.auto_close = Some((open, close));
        let saved = (self.font, self.bold);
        let mut parts: Vec<Vec<Node>> = Vec::new();
        loop {
            let (nodes, stop) = self.list()?;
            match stop {
                Stop::AutoClose => {
                    parts.push(nodes);
                    break;
                }
                // A line break here has nowhere to go, as in `fenced`. The next list is this one still.
                Stop::Cr => {
                    parts.push(nodes);
                    self.auto_close = Some((open, close));
                }
                Stop::Eof | Stop::Close | Stop::End(_) => return Err("Extra open or missing close delimiter".to_owned()),
                Stop::Amp | Stop::Cd(_) => return Err("Misplaced &".to_owned()),
                Stop::Right => return Err("Missing \\left or extra \\right".to_owned()),
                Stop::Middle => return Err("Extra \\middle".to_owned()),
            }
        }
        (self.font, self.bold) = saved;
        if ignore {
            return Ok(None);
        }
        let mut inner: Vec<Node> = parts.into_iter().flatten().collect();
        if smash {
            inner = vec![Node::Smash { body: inner }];
        }
        if phantom {
            inner.extend(self.parse_string("\\vphantom{\\int}")?);
        }
        Ok(Some(match big {
            Some(big) => {
                let height = big_delimiter(big).map_or(1.2, |(height, _)| height);
                Node::Group(vec![Node::Big { delim: open, height, class: Class::Open }, Node::Group(inner), Node::Big { delim: close, height, class: Class::Close }])
            }
            None => Node::Fenced { left: open, parts: vec![inner], middles: Vec::new(), right: close },
        }))
    }

    /// `PhysicsMethods.Quantity`.
    fn quantity(&mut self, name: &str, open: &str, close: &str, arg: bool, named: bool, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let star = arg && self.star();
        self.skip_space();
        let position = self.at;
        let empty = || Node::Fenced { left: fence_char(open), parts: vec![Vec::new()], middles: Vec::new(), right: fence_char(close) };
        let mut big = None;
        if self.peek() == Some('\\') {
            self.bump();
            let cs = self.control_name();
            if !is_biggs(&cs) {
                nodes.push(empty());
                self.at = position;
                return Ok(true);
            }
            big = Some(cs);
            self.skip_space();
        }
        let next = self.peek();
        if arg && next != Some('{') {
            return Err(format!("Missing argument for \\{name}"));
        }
        let right = match next {
            Some('(') => ')',
            Some('[') => ']',
            Some('{') => '}',
            Some('|') => '|',
            _ => {
                nodes.push(empty());
                self.at = position;
                return Ok(true);
            }
        };
        if named {
            nodes.push(Node::Op { body: Box::new(Node::Sym { text: "O".to_owned(), class: Class::Ord, font: MathFont::Script, bold: false }), big: false, integral: false, display_limits: false, limits: None });
        }
        if next == Some('{') {
            let argument = self.get_arg(name)?;
            let (open, close) = if arg { (open, close) } else { ("\\{", "\\}") };
            let tex = if star {
                format!("{open} {argument} {close}")
            } else if let Some(big) = &big {
                format!("\\{big}l{open} {argument} \\{big}r{close}")
            } else {
                format!("\\left{open} {argument} \\right{close}")
            };
            return self.parsed(&tex, nodes);
        }
        self.bump();
        let open = next.expect("just matched");
        if let Some(node) = self.auto_open(open, right, big.as_deref(), false, false, false)? {
            nodes.push(node);
        }
        Ok(true)
    }

    fn commutator(&mut self, name: &str, open: &str, close: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let star = self.star();
        self.skip_space();
        let mut big = None;
        if self.peek() == Some('\\') {
            self.bump();
            let cs = self.control_name();
            if !is_biggs(&cs) {
                return Err(format!("Missing argument for \\{name}"));
            }
            big = Some(cs);
            self.skip_space();
        }
        if self.peek() != Some('{') {
            return Err(format!("Missing argument for \\{name}"));
        }
        let first = self.get_arg(name)?;
        let second = self.get_arg(name)?;
        let argument = format!("{first},{second}");
        let tex = if star {
            format!("{open} {argument} {close}")
        } else if let Some(big) = big {
            format!("\\{big}l{open} {argument} \\{big}r{close}")
        } else {
            format!("\\left{open} {argument} \\right{close}")
        };
        self.parsed(&tex, nodes)
    }

    /// `vectorApplication`: an operator, and what its fences do to what comes next.
    fn apply_operator(&mut self, name: &str, operator: &str, fences: &[char], nodes: &mut Vec<Node>) -> Parsed<bool> {
        self.parsed(operator, nodes)?;
        self.skip_space();
        let Some(left) = self.peek() else { return Ok(true) };
        let right = match left {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            '|' => '|',
            _ => return Ok(true),
        };
        let enlarge = fences.contains(&left);
        if left == '{' {
            let arg = self.get_arg(name)?;
            self.splice(&if enlarge { format!("\\left\\{{ {arg} \\right\\}}") } else { format!(" {arg} ") })?;
            return Ok(true);
        }
        if !enlarge {
            return Ok(true);
        }
        self.bump();
        if let Some(node) = self.auto_open(left, right, None, false, false, false)? {
            nodes.push(node);
        }
        Ok(true)
    }

    /// `PhysicsMethods.Expression`: `\sin`, with its `[2]` and its stretching `(`.
    fn expression(&mut self, name: &str, optional: bool, shown: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let power = if optional { self.get_brackets(name)? } else { None };
        let op = Node::Op { body: Box::new(Node::Sym { text: shown.to_owned(), class: Class::Ord, font: MathFont::Roman, bold: false }), big: false, integral: false, display_limits: false, limits: None };
        nodes.push(match power {
            Some(power) => {
                let sup = self.parse_string(&power)?;
                Node::Scripts { base: Box::new(op), sup: Some(Box::new(Node::Group(sup))), sub: None, primes: 0 }
            }
            None => op,
        });
        self.skip_space();
        if self.peek() == Some('(') {
            self.bump();
            if let Some(node) = self.auto_open('(', ')', None, false, false, false)? {
                nodes.push(node);
            }
        }
        Ok(true)
    }

    fn named_fn(&mut self, shown: &str, nodes: &mut Vec<Node>) -> bool {
        nodes.push(Node::Op { body: Box::new(Node::Sym { text: shown.to_owned(), class: Class::Ord, font: MathFont::Roman, bold: false }), big: false, integral: false, display_limits: false, limits: None });
        true
    }

    fn qqtext(&mut self, name: &str, text: Option<&str>) -> Parsed<bool> {
        let star = self.star();
        let arg = match text {
            Some(text) => text.to_owned(),
            None => self.get_arg(name)?,
        };
        let lead = if star { "" } else { "\\quad" };
        self.splice(&format!("{lead}\\text{{{arg}}}\\quad "))?;
        Ok(true)
    }

    fn differential(&mut self, name: &str, op: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let power = self.get_brackets(name)?.map_or(" ".to_owned(), |p| format!("^{{{p}}}"));
        self.skip_space();
        let tex = format!("{op}{power}");
        match self.peek() {
            Some('(') => {
                self.parsed(&tex, nodes)?;
                self.bump();
                if let Some(node) = self.auto_open('(', ')', None, false, false, false)? {
                    nodes.push(node);
                }
                Ok(true)
            }
            Some('{') => {
                let arg = self.get_arg(name)?;
                let body = self.parse_string(&format!("{tex}{arg}"))?;
                nodes.push(Node::Classed(Class::Op, body));
                Ok(true)
            }
            None => self.parsed(&tex, nodes),
            _ => {
                let arg = self.get_arg(name)?;
                self.parsed(&format!("{tex}{arg}"), nodes)
            }
        }
    }

    fn derivative(&mut self, name: &str, most: usize, op: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let star = self.star();
        let power = self.get_brackets(name)?;
        let mut args = vec![self.get_arg(name)?];
        while self.next_is('{') && args.len() < most {
            args.push(self.get_arg(name)?);
        }
        let mut ignore = false;
        let (mut power1, mut power2) = (" ".to_owned(), " ".to_owned());
        if most > 2 && args.len() > 2 {
            power1 = format!("^{{{}}}", args.len() - 1);
            ignore = true;
        } else if let Some(power) = power {
            if most > 2 && args.len() > 1 {
                ignore = true;
            }
            power1 = format!("^{{{power}}}");
            power2 = power1.clone();
        }
        let frac = if star { "\\flatfrac" } else { "\\frac" };
        let first = if args.len() > 1 { args[0].as_str() } else { "" };
        let second = if args.len() > 1 { args[1].as_str() } else { args[0].as_str() };
        let rest: String = args.iter().skip(2).map(|arg| format!("{op} {arg}")).collect();
        self.parsed(&format!("{frac}{{{op}{power1}{first}}}{{{op} {second}{power2} {rest}}}"), nodes)?;
        self.skip_space();
        if self.peek() == Some('(') {
            self.bump();
            if let Some(node) = self.auto_open('(', ')', None, false, false, ignore)? {
                nodes.push(node);
            }
        }
        Ok(true)
    }

    fn physics_bra(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let star_bra = self.star();
        let bra = self.get_arg(name)?;
        let (mut ket, mut has_ket, mut star_ket) = (String::new(), false, false);
        self.skip_space();
        if self.peek() == Some('\\') {
            let save = self.at;
            self.bump();
            let cs = self.control_name();
            if cs == "ket" {
                has_ket = true;
                let after = self.at;
                star_ket = self.star();
                if self.next_is('{') {
                    ket = self.get_arg(&cs)?;
                } else {
                    self.at = after;
                    star_ket = false;
                }
            } else {
                self.at = save;
            }
        }
        let tex = if has_ket {
            if star_bra || star_ket {
                format!("\\langle{{{bra}}}\\vert{{{ket}}}\\rangle")
            } else {
                format!("\\left\\langle{{{bra}}}\\middle\\vert{{{ket}}}\\right\\rangle")
            }
        } else if star_bra {
            format!("\\langle{{{bra}}}\\vert")
        } else {
            format!("\\left\\langle{{{bra}}}\\right\\vert{{{ket}}}")
        };
        self.parsed(&tex, nodes)
    }

    fn matrix_quantity(&mut self, name: &str, small: bool, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let star = self.star();
        self.skip_space();
        let array = if small { "smallmatrix" } else { "array" };
        let (mut arg, mut open, mut close) = (String::new(), "", "");
        match self.peek() {
            Some('{') => arg = self.get_arg(name)?,
            Some('(') => {
                self.bump();
                (open, close) = if star { ("\\lgroup", "\\rgroup") } else { ("(", ")") };
                arg = self.get_up_to(name, ')')?;
            }
            Some('[') => {
                self.bump();
                (open, close) = ("[", "]");
                arg = self.get_up_to(name, ']')?;
            }
            Some('|') => {
                self.bump();
                (open, close) = ("|", "|");
                arg = self.get_up_to(name, '|')?;
            }
            _ => (open, close) = ("(", ")"),
        }
        let tex = if open.is_empty() {
            format!("\\begin{{{array}}}{{}} {arg}\\end{{{array}}}")
        } else {
            format!("\\left{open}\\begin{{{array}}}{{}} {arg}\\end{{{array}}}\\right{close}")
        };
        self.parsed(&tex, nodes)
    }

    fn matrix_text(&mut self, name: &str) -> Parsed<bool> {
        let tex = match name {
            "identitymatrix" | "imat" => {
                let arg = self.get_arg(name)?;
                let size: i64 = arg.trim().parse().map_err(|_| "Invalid number".to_owned())?;
                if size <= 1 {
                    "1".to_owned()
                } else {
                    let size = size.min(MAX_COLUMNS as i64) as usize;
                    (0..size).map(|i| (0..size).map(|j| if i == j { "1" } else { "0" }).collect::<Vec<_>>().join(" & ")).collect::<Vec<_>>().join("\\\\ ")
                }
            }
            "xmatrix" | "xmat" => {
                let star = self.star();
                let cell = self.get_arg(name)?;
                let (n, m) = (self.get_arg(name)?, self.get_arg(name)?);
                let parse = |raw: &str| raw.parse::<i64>().ok().filter(|v| v.to_string() == raw);
                let (Some(n), Some(m)) = (parse(&n), parse(&m)) else { return Err("Invalid number".to_owned()) };
                let (n, m) = (n.clamp(1, MAX_COLUMNS as i64), m.clamp(1, MAX_COLUMNS as i64));
                if !star {
                    let row = vec![cell.as_str(); m as usize].join(" & ");
                    vec![row; n as usize].join("\\\\ ")
                } else if n == 1 && m == 1 {
                    cell
                } else if n == 1 {
                    (1..=m).map(|i| format!("{cell}_{{{i}}}")).collect::<Vec<_>>().join(" & ")
                } else if m == 1 {
                    (1..=n).map(|i| format!("{cell}_{{{i}}}")).collect::<Vec<_>>().join("\\\\ ")
                } else {
                    (1..=n).map(|i| (1..=m).map(|j| format!("{cell}_{{{{{i}}}{{{j}}}}}")).collect::<Vec<_>>().join(" & ")).collect::<Vec<_>>().join("\\\\ ")
                }
            }
            "paulimatrix" | "pmat" => {
                let arg = self.get_arg(name)?;
                let mut chars = arg.chars();
                let first = chars.next();
                let rest = chars.as_str().to_owned();
                match first {
                    Some('0') => format!("{rest} 1 & 0\\\\ 0 & 1"),
                    Some('1' | 'x') => format!("{rest} 0 & 1\\\\ 1 & 0"),
                    Some('2' | 'y') => format!("{rest} 0 & -i\\\\ i & 0"),
                    Some('3' | 'z') => format!("{rest} 1 & 0\\\\ 0 & -1"),
                    _ => rest,
                }
            }
            _ => {
                let anti = matches!(name, "antidiagonalmatrix" | "admat");
                self.skip_space();
                if self.peek() != Some('{') {
                    return Ok(true);
                }
                let arg = self.get_arg(name)?;
                let mut elements = Vec::new();
                let (mut depth, mut start) = (0, 0);
                for (at, c) in arg.char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        ',' if depth == 0 => {
                            elements.push(arg[start..at].to_owned());
                            start = at + 1;
                        }
                        _ => {}
                    }
                }
                elements.push(arg[start..].to_owned());
                let length = elements.len();
                elements.iter().enumerate().map(|(i, element)| format!("{}\\mqty{{{element}}}", "&".repeat(if anti { length - i - 1 } else { i }))).collect::<Vec<_>>().join("\\\\ ")
            }
        };
        self.splice(&tex)?;
        Ok(true)
    }

    fn vector_bold(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let star = self.star();
        let raw = self.get_arg(name)?;
        let saved = (self.font, self.bold, self.vector);
        self.font = if star { MathFont::BoldItalic } else { MathFont::Bold };
        self.vector = Some(star);
        let parsed = self.parse_string(&raw);
        (self.font, self.bold, self.vector) = saved;
        nodes.push(Node::Group(parsed?));
        Ok(true)
    }

    fn physics(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let symbol = |text: &str, class| Node::Sym { text: text.to_owned(), class, font: MathFont::Roman, bold: false };
        match name {
            "quantity" | "qty" => self.quantity(name, "(", ")", false, false, nodes),
            "pqty" => self.quantity(name, "(", ")", true, false, nodes),
            "bqty" => self.quantity(name, "[", "]", true, false, nodes),
            "vqty" | "absolutevalue" | "abs" => self.quantity(name, "|", "|", true, false, nodes),
            "Bqty" => self.quantity(name, "\\{", "\\}", true, false, nodes),
            "norm" => self.quantity(name, "\\|", "\\|", true, false, nodes),
            "order" => self.quantity(name, "(", ")", true, true, nodes),
            "evaluated" | "eval" => {
                let star = self.star();
                self.skip_space();
                match self.peek() {
                    Some(open @ ('(' | '[')) => {
                        self.bump();
                        if let Some(node) = self.auto_open(open, '|', None, star, true, false)? {
                            nodes.push(node);
                        }
                        Ok(true)
                    }
                    Some('{') => {
                        let arg = self.get_arg(name)?;
                        let arg = if star { format!("\\smash{{{arg}}}") } else { arg };
                        self.splice(&format!("\\left.{arg}\\vphantom{{\\int}}\\right|"))?;
                        Ok(true)
                    }
                    _ => {
                        self.splice("\\left.\\vphantom{\\int}\\right|")?;
                        Ok(true)
                    }
                }
            }
            "commutator" | "comm" => self.commutator(name, "[", "]", nodes),
            "anticommutator" | "acomm" | "poissonbracket" | "pb" => self.commutator(name, "\\{", "\\}", nodes),
            "dotproduct" | "vdot" => {
                nodes.push(Node::Sym { text: "⋅".to_owned(), class: Class::Bin, font: MathFont::Bold, bold: true });
                Ok(true)
            }
            "crossproduct" | "cross" | "cp" => {
                nodes.push(symbol("×", Class::Bin));
                Ok(true)
            }
            "gradientnabla" => {
                nodes.push(Node::Sym { text: "∇".to_owned(), class: Class::Ord, font: MathFont::Bold, bold: true });
                Ok(true)
            }
            "divsymbol" | "divisionsymbol" => {
                nodes.push(symbol("÷", Class::Bin));
                Ok(true)
            }
            "real" => {
                nodes.push(symbol("ℜ", Class::Ord));
                Ok(true)
            }
            "imaginary" => {
                nodes.push(symbol("ℑ", Class::Ord));
                Ok(true)
            }
            "vnabla" => self.parsed("{\\gradientnabla}", nodes),
            "vectorbold" | "vb" => self.vector_bold(name, nodes),
            "vectorarrow" | "va" | "vectorunit" | "vu" => {
                let star = if self.star() { "*" } else { "" };
                let arg = self.get_arg(name)?;
                let accent = if matches!(name, "vectorarrow" | "va") { "vec" } else { "hat" };
                self.splice(&format!("\\{accent}{{\\vb{star}{{{arg}}}}}"))?;
                Ok(true)
            }
            "gradient" | "grad" => self.apply_operator(name, "\\vnabla", &['(', '['], nodes),
            "divergence" | "div" => self.apply_operator(name, "\\vnabla\\vdot", &['(', '['], nodes),
            "curl" => self.apply_operator(name, "\\vnabla\\crossproduct", &['(', '['], nodes),
            "laplacian" => self.apply_operator(name, "\\nabla^2", &['(', '['], nodes),
            "sin" | "sinh" | "arcsin" | "asin" | "cos" | "cosh" | "arccos" | "acos" | "tan" | "tanh" | "arctan" | "atan" | "csc" | "csch" | "arccsc" | "acsc" | "sec" | "sech" | "arcsec" | "asec" | "cot" | "coth" | "arccot" | "acot" | "log" | "ln" => self.expression(name, true, name, nodes),
            "exp" | "det" | "Pr" | "tr" | "Tr" | "erf" => self.expression(name, false, name, nodes),
            "trace" => self.expression(name, false, "tr", nodes),
            "Trace" => self.expression(name, false, "Tr", nodes),
            "rank" => Ok(self.named_fn("rank", nodes)),
            "Residue" => self.parsed("\\mathrm{Res}", nodes),
            "Res" => self.apply_operator(name, "\\Residue", &['(', '[', '{'], nodes),
            "principalvalue" | "pv" => self.apply_operator(name, "{\\cal P}", &[], nodes),
            "PV" => self.apply_operator(name, "{\\rm P.V.}", &[], nodes),
            "Re" => self.apply_operator(name, "\\mathrm{Re}", &['{'], nodes),
            "Im" => self.apply_operator(name, "\\mathrm{Im}", &['{'], nodes),
            "sine" => Ok(self.named_fn("sin", nodes)),
            "hypsine" => Ok(self.named_fn("sinh", nodes)),
            "arcsine" => Ok(self.named_fn("arcsin", nodes)),
            "asine" => Ok(self.named_fn("asin", nodes)),
            "cosine" => Ok(self.named_fn("cos", nodes)),
            "hypcosine" => Ok(self.named_fn("cosh", nodes)),
            "arccosine" => Ok(self.named_fn("arccos", nodes)),
            "acosine" => Ok(self.named_fn("acos", nodes)),
            "tangent" => Ok(self.named_fn("tan", nodes)),
            "hyptangent" => Ok(self.named_fn("tanh", nodes)),
            "arctangent" => Ok(self.named_fn("arctan", nodes)),
            "atangent" => Ok(self.named_fn("atan", nodes)),
            "cosecant" => Ok(self.named_fn("csc", nodes)),
            "hypcosecant" => Ok(self.named_fn("csch", nodes)),
            "arccosecant" => Ok(self.named_fn("arccsc", nodes)),
            "acosecant" => Ok(self.named_fn("acsc", nodes)),
            "secant" => Ok(self.named_fn("sec", nodes)),
            "hypsecant" => Ok(self.named_fn("sech", nodes)),
            "arcsecant" => Ok(self.named_fn("arcsec", nodes)),
            "asecant" => Ok(self.named_fn("asec", nodes)),
            "cotangent" => Ok(self.named_fn("cot", nodes)),
            "hypcotangent" => Ok(self.named_fn("coth", nodes)),
            "arccotangent" => Ok(self.named_fn("arccot", nodes)),
            "acotangent" => Ok(self.named_fn("acot", nodes)),
            "exponential" => Ok(self.named_fn("exp", nodes)),
            "logarithm" => Ok(self.named_fn("log", nodes)),
            "naturallogarithm" => Ok(self.named_fn("ln", nodes)),
            "determinant" => Ok(self.named_fn("det", nodes)),
            "Probability" => Ok(self.named_fn("Pr", nodes)),
            "qqtext" | "qq" => self.qqtext(name, None),
            "qcomma" | "qc" => {
                self.splice("\\qqtext*{,}")?;
                Ok(true)
            }
            "qcc" => self.qqtext(name, Some("c.c.")),
            "qif" | "qthen" | "qelse" | "qotherwise" | "qunless" | "qgiven" | "qusing" | "qassume" | "qsince" | "qlet" | "qfor" | "qall" | "qeven" | "qodd" | "qinteger" | "qand" | "qor" | "qas" | "qin" => {
                let word = &name[1..];
                self.qqtext(name, Some(word))
            }
            "diffd" => self.parsed("{\\rm d}", nodes),
            "flatfrac" => {
                let num = self.get_arg(name)?;
                let den = self.get_arg(name)?;
                self.splice(&format!("\\left.{{{num}}}\\middle/{{{den}}}\\right."))?;
                Ok(true)
            }
            "differential" | "dd" => self.differential(name, "\\diffd", nodes),
            "variation" | "var" => self.differential(name, "\\delta", nodes),
            "derivative" | "dv" => self.derivative(name, 2, "\\diffd", nodes),
            "partialderivative" | "pderivative" | "pdv" => self.derivative(name, 3, "\\partial", nodes),
            "functionalderivative" | "fderivative" | "fdv" => self.derivative(name, 2, "\\delta", nodes),
            "bra" => self.physics_bra(name, nodes),
            "ket" => {
                let star = self.star();
                let ket = self.get_arg(name)?;
                let tex = if star { format!("\\vert{{{ket}}}\\rangle") } else { format!("\\left\\vert{{{ket}}}\\right\\rangle") };
                self.parsed(&tex, nodes)
            }
            "innerproduct" | "ip" | "braket" => {
                let star = self.star();
                let bra = self.get_arg(name)?;
                let ket = if self.next_is('{') { self.get_arg(name)? } else { bra.clone() };
                let tex = if star { format!("\\langle{{{bra}}}\\vert{{{ket}}}\\rangle") } else { format!("\\left\\langle{{{bra}}}\\middle\\vert{{{ket}}}\\right\\rangle") };
                self.parsed(&tex, nodes)
            }
            "outerproduct" | "dyad" | "ketbra" | "op" => {
                let star = self.star();
                let ket = self.get_arg(name)?;
                let bra = if self.next_is('{') { self.get_arg(name)? } else { ket.clone() };
                let tex = if star { format!("\\vert{{{ket}}}\\rangle\\!\\langle{{{bra}}}\\vert") } else { format!("\\left\\vert{{{ket}}}\\middle\\rangle\\!\\middle\\langle{{{bra}}}\\right\\vert") };
                self.parsed(&tex, nodes)
            }
            "expectationvalue" | "expval" | "ev" => {
                let star1 = self.star();
                let star2 = star1 && self.star();
                let first = self.get_arg(name)?;
                let second = if self.next_is('{') { Some(self.get_arg(name)?) } else { None };
                let tex = match second {
                    Some(second) if !first.is_empty() && !second.is_empty() => bracket_text([&second, &first, &second], star1, star2),
                    _ if star1 => format!("\\langle {{{first}}} \\rangle"),
                    _ => format!("\\left\\langle {{{first}}} \\right\\rangle"),
                };
                self.parsed(&tex, nodes)
            }
            "matrixelement" | "matrixel" | "mel" => {
                let star1 = self.star();
                let star2 = star1 && self.star();
                let (a, b, c) = (self.get_arg(name)?, self.get_arg(name)?, self.get_arg(name)?);
                self.parsed(&bracket_text([&a, &b, &c], star1, star2), nodes)
            }
            "matrixquantity" | "mqty" => self.matrix_quantity(name, false, nodes),
            "smallmatrixquantity" | "smqty" => self.matrix_quantity(name, true, nodes),
            "pmqty" | "Pmqty" | "bmqty" | "vmqty" | "spmqty" | "sPmqty" | "sbmqty" | "svmqty" | "matrixdeterminant" | "mdet" | "smdet" => {
                let arg = self.get_arg(name)?;
                let tex = match name {
                    "pmqty" => format!("\\mqty({arg})"),
                    "Pmqty" => format!("\\mqty*({arg})"),
                    "bmqty" => format!("\\mqty[{arg}]"),
                    "vmqty" => format!("\\mqty|{arg}|"),
                    "spmqty" => format!("\\smqty({arg})"),
                    "sPmqty" => format!("\\smqty*({arg})"),
                    "sbmqty" => format!("\\smqty[{arg}]"),
                    "svmqty" => format!("\\smqty|{arg}|"),
                    "smdet" => format!("\\svmqty{{{arg}}}"),
                    _ => format!("\\vmqty{{{arg}}}"),
                };
                self.splice(&tex)?;
                Ok(true)
            }
            "zeromatrix" | "zmat" => {
                let (n, m) = (self.get_arg(name)?, self.get_arg(name)?);
                self.splice(&format!("\\xmat{{0}}{{{n}}}{{{m}}}"))?;
                Ok(true)
            }
            "identitymatrix" | "imat" | "xmatrix" | "xmat" | "paulimatrix" | "pmat" | "diagonalmatrix" | "dmat" | "antidiagonalmatrix" | "admat" => self.matrix_text(name),
            _ => Ok(false),
        }
    }

    // ------------------------------------------------------------ mhchem

    fn mhchem(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let simple = match name {
            "ce" | "pu" => {
                let raw = self.get_arg(name)?;
                let tex = mhchem::to_tex(&raw, name == "pu")?;
                return self.parsed(&tex, nodes);
            }
            "mhchemxrightarrow" => Some("\\xrightarrow"),
            "mhchemxleftarrow" => Some("\\xleftarrow"),
            "mhchemxleftrightarrow" => Some("\\xleftrightarrow"),
            "mhchemxleftrightarrows" => Some("\\xtofrom"),
            "mhchemxrightleftharpoons" | "mhchemxRightleftharpoons" | "mhchemxLeftrightharpoons" => Some("\\xrightleftharpoons"),
            _ => None,
        };
        if let Some(target) = simple {
            self.splice(target)?;
            return Ok(true);
        }
        let symbol = match name {
            "tripledash" => "\u{2504}",
            "mhchemBondTD" | "mhchemBondTDD" | "mhchemBondDTD" => "\u{2261}",
            "mhchemlongleftarrow" => "⟵",
            "mhchemleftarrow" => "←",
            "mhchemlongrightarrow" => "⟶",
            "mhchemrightarrow" => "→",
            "mhchemlongleftrightarrow" => "⟷",
            "mhchemleftrightarrow" => "↔",
            "mhchemlongrightleftharpoons" | "mhchemlongRightleftharpoons" | "mhchemlongLeftrightharpoons" => "⇌",
            "mhchemlongleftrightarrows" => "⇄",
            _ => return Ok(false),
        };
        nodes.push(Node::Sym { text: symbol.to_owned(), class: Class::Rel, font: MathFont::Roman, bold: false });
        Ok(true)
    }

    // ------------------------------------------------------------ mathtools

    /// `\mathllap[\scriptstyle]{...}`'s bracket.
    fn style_option(&mut self, name: &str) -> Parsed<Option<MathStyle>> {
        Ok(match self.get_brackets(name)?.as_deref().map(str::trim) {
            Some("\\displaystyle") => Some(MathStyle::Display),
            Some("\\textstyle") => Some(MathStyle::Text),
            Some("\\scriptstyle") => Some(MathStyle::Script),
            Some("\\scriptscriptstyle") => Some(MathStyle::ScriptScript),
            _ => None,
        })
    }

    fn math_lap(&mut self, name: &str, side: i8, cramped: bool, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let style = self.style_option(name)?;
        let body = self.parse_arg(name)?;
        let body = if cramped || style.is_some() { vec![Node::Cramped { style, body }] } else { body };
        nodes.push(Node::Lap { side, body });
        Ok(true)
    }

    fn paired(&mut self, name: &str, open: &str, close: &str, body: &str, args: u8, pre: &str, post: &str) -> Parsed<()> {
        let star = self.star();
        let size = if star { None } else { self.get_brackets(name)? };
        let (left, right, after) = if star {
            ("\\mathopen{\\left".to_owned(), "\\right".to_owned(), "}\\mathclose{}")
        } else if let Some(size) = &size {
            (format!("{size}l"), format!("{size}r"), "")
        } else {
            (String::new(), String::new(), "")
        };
        let delim = if star { "\\middle".to_owned() } else { size.unwrap_or_default() };
        let mut values = Vec::with_capacity(args as usize);
        for _ in 0..args {
            values.push(self.get_arg(name)?);
        }
        let (pre, body, post) = (substitute(pre, &values), substitute(body, &values).replace("\\delimsize", &delim), substitute(post, &values));
        self.splice(&format!("{pre}{left}{open} {body}{right}{close}{after}{post}"))
    }

    fn aboxed(&mut self, name: &str, boxed: &str, math: bool) -> Parsed<()> {
        let Some(cell) = &self.cell else { return Err(format!("{name} can only be used in aligned environments")) };
        if !cell.aligned {
            return Err(format!("{name} can only be used in aligned environments"));
        }
        let lead = if cell.column % 2 == 1 { "&" } else { "" };
        let arg = self.get_arg(name)?;
        let (left, right) = match split_at_amp(&arg) {
            Some((left, right)) => (left.to_owned(), right.to_owned()),
            None => (arg.clone(), String::new()),
        };
        let (open, close) = if math { ("", "") } else { ("$\\displaystyle{", "}$") };
        self.splice(&format!("{lead}\\rlap{{\\{boxed}{{{open}{{{left}}}{{}}{{{right}}}{close}}}}}\\kern.267em\\phantom{{{left}}}&\\phantom{{{{}}{right}}}\\kern.267em"))
    }

    fn check_alignment(&self, name: &str) -> Parsed<()> {
        if self.cell.as_ref().is_some_and(|c| c.aligned) { Ok(()) } else { Err(format!("\\{name} can only be used in aligned environments")) }
    }

    fn mathtools(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        match name {
            "shoveleft" | "shoveright" => {
                let Some(cell) = &mut self.cell else { return Err(format!("\\{name} can only appear within the multline or multlined environments")) };
                if !cell.multline {
                    return Err(format!("\\{name} can only appear within the multline or multlined environments"));
                }
                cell.shove = Some(if name == "shoveleft" { 'l' } else { 'r' });
                let shift = self.get_brackets(name)?.and_then(|raw| dimension(&raw));
                let body = self.parse_arg(name)?;
                let mut out = Vec::new();
                if name == "shoveleft" {
                    out.extend(shift.map(Node::Space));
                }
                out.push(Node::Group(body));
                if name == "shoveright" {
                    out.extend(shift.map(Node::Space));
                }
                nodes.push(Node::Group(out));
                Ok(true)
            }
            "mathllap" => self.math_lap(name, -1, false, nodes),
            "mathrlap" => self.math_lap(name, 1, false, nodes),
            "mathclap" => self.math_lap(name, 0, false, nodes),
            "crampedllap" => self.math_lap(name, -1, true, nodes),
            "crampedrlap" => self.math_lap(name, 1, true, nodes),
            "crampedclap" => self.math_lap(name, 0, true, nodes),
            "clap" | "textllap" | "textrlap" | "textclap" => {
                let raw = self.get_arg(name)?;
                let body = self.parse_text(&raw, MathFont::Roman)?;
                let side = match name {
                    "textllap" => -1,
                    "textrlap" => 1,
                    _ => 0,
                };
                nodes.push(Node::Lap { side, body });
                Ok(true)
            }
            "cramped" => {
                let style = self.style_option(name)?;
                let body = self.parse_arg(name)?;
                nodes.push(Node::Cramped { style, body });
                Ok(true)
            }
            "crampedsubstack" => {
                let arg = self.get_arg(name)?;
                self.splice(&format!("\\begin{{crampedsubarray}}{{c}}{arg}\\end{{crampedsubarray}}"))?;
                Ok(true)
            }
            "mathmbox" => {
                nodes.push(Node::Group(self.parse_arg(name)?));
                Ok(true)
            }
            "mathmakebox" => {
                let width = self.get_brackets(name)?.and_then(|raw| dimension(&raw));
                let align = position(self.get_brackets(name)?.as_deref());
                let body = self.parse_arg(name)?;
                nodes.push(Node::Sized { width, align, body });
                Ok(true)
            }
            "overbracket" | "underbracket" => {
                let thick = self.get_brackets(name)?.and_then(|raw| dimension(&raw)).unwrap_or(0.1);
                let height = self.get_brackets(name)?.and_then(|raw| dimension(&raw)).unwrap_or(0.2);
                let body = self.parse_arg(name)?;
                let kind = if name == "overbracket" { OverKind::OverBracket { thick, height } } else { OverKind::UnderBracket { thick, height } };
                nodes.push(Node::Op { body: Box::new(Node::Over { kind, body }), big: false, integral: false, display_limits: true, limits: Some(true) });
                Ok(true)
            }
            "refeq" => {
                let label = self.get_arg(name)?;
                nodes.push(self.reference(&label, false));
                Ok(true)
            }
            "MoveEqLeft" => {
                self.expand_with(name, 1, Some("2"), "\\hspace{#1em}&\\hspace{-#1em}")?;
                Ok(true)
            }
            "Aboxed" => {
                self.aboxed(name, "boxed", true)?;
                Ok(true)
            }
            "MakeAboxedCommand" => {
                let star = self.star();
                let target = self.cs_name_argument(name)?;
                let boxed = self.cs_name_argument(name)?;
                self.define(target, Macro::Aboxed { boxed, math: !star });
                Ok(true)
            }
            "ArrowBetweenLines" => {
                self.check_alignment(name)?;
                let star = self.star();
                let symbol = self.get_brackets(name)?.unwrap_or_else(|| "\\Updownarrow".to_owned());
                let tex = if star { format!("&&\\quad{symbol}\\\\") } else { format!("{symbol}\\quad\\\\") };
                self.splice(&tex)?;
                Ok(true)
            }
            "vdotswithin" => {
                let arg = self.get_arg(name)?;
                let base = self.parse_string(&format!("{{}}{arg}{{}}"))?;
                nodes.push(Node::CenterOver { base: vec![Node::Phantom { width: true, height: true, body: base }], over: vec![Node::Sym { text: "⋮".to_owned(), class: Class::Ord, font: MathFont::Roman, bold: false }] });
                Ok(true)
            }
            "shortvdotswithin" => {
                self.check_alignment(name)?;
                let star = self.star();
                let arg = self.get_arg(name)?;
                let tex = if star { format!("\\vdotswithin{{{arg}}}&\\\\") } else { format!("&\\vdotswithin{{{arg}}}\\\\") };
                self.splice(&tex)?;
                Ok(true)
            }
            "MTFlushSpaceAbove" => {
                self.check_alignment(name)?;
                Ok(true)
            }
            "MTFlushSpaceBelow" => {
                self.check_alignment(name)?;
                self.splice("\\\\")?;
                Ok(true)
            }
            "DeclarePairedDelimiter" | "DeclarePairedDelimiters" => {
                let target = self.cs_name_argument(name)?;
                let (open, close) = (self.get_arg(name)?, self.get_arg(name)?);
                self.define(target, Macro::Paired { open, close, body: "#1".to_owned(), args: 1, pre: String::new(), post: String::new() });
                Ok(true)
            }
            "DeclarePairedDelimiterX" | "DeclarePairedDelimitersX" => {
                let target = self.cs_name_argument(name)?;
                let args = self.arg_count(name)?;
                let (open, close, body) = (self.get_arg(name)?, self.get_arg(name)?, self.get_arg(name)?);
                self.define(target, Macro::Paired { open, close, body, args, pre: String::new(), post: String::new() });
                Ok(true)
            }
            "DeclarePairedDelimiterXPP" | "DeclarePairedDelimitersXPP" => {
                let target = self.cs_name_argument(name)?;
                let args = self.arg_count(name)?;
                let (pre, open, close, post, body) = (self.get_arg(name)?, self.get_arg(name)?, self.get_arg(name)?, self.get_arg(name)?, self.get_arg(name)?);
                self.define(target, Macro::Paired { open, close, body, args, pre, post });
                Ok(true)
            }
            "vcentercolon" | "MTThinColon" | "ordinarycolon" => {
                let colon = Node::Sym { text: ":".to_owned(), class: Class::Rel, font: MathFont::Roman, bold: false };
                nodes.push(match name {
                    "ordinarycolon" => colon,
                    "vcentercolon" => Node::Raise { by: 0.04, body: vec![colon] },
                    _ => Node::Classed(Class::Rel, vec![Node::Space(-0.04), Node::Raise { by: 0.04, body: vec![colon] }, Node::Space(-0.04)]),
                });
                Ok(true)
            }
            "coloneqq" | "coloneq" => self.relation(":="),
            "Coloneqq" | "Coloneq" => self.relation("::="),
            "eqqcolon" | "eqcolon" => self.relation("=:"),
            "Eqqcolon" | "Eqcolon" => self.relation("=::"),
            "colonapprox" => self.relation(":\\approx"),
            "Colonapprox" => self.relation("::\\approx"),
            "colonsim" => self.relation(":\\sim"),
            "Colonsim" => self.relation("::\\sim"),
            "dblcolon" => self.relation("::"),
            "approxcolon" => self.relation("\\approx:"),
            "Approxcolon" => self.relation("\\approx::"),
            "simcolon" => self.relation("\\sim:"),
            "Simcolon" => self.relation("\\sim::"),
            "colondash" => self.relation(":-"),
            "Colondash" => self.relation("::-"),
            "dashcolon" => self.relation("-:"),
            "Dashcolon" => self.relation("-::"),
            "nuparrow" | "ndownarrow" => {
                let text = if name == "nuparrow" { "↑" } else { "↓" };
                nodes.push(Node::Not(Box::new(Node::Sym { text: text.to_owned(), class: Class::Rel, font: MathFont::Roman, bold: false })));
                Ok(true)
            }
            "bigtimes" => {
                self.splice("\\mathop{\\Large\\kern-.1em\\boldsymbol{\\times}\\kern-.1em}")?;
                Ok(true)
            }
            "splitfrac" | "splitdfrac" => {
                let mut num = self.parse_arg(name)?;
                let mut den = self.parse_arg(name)?;
                num.push(Node::Space(1.0));
                den.insert(0, Node::Space(1.0));
                let style = if name == "splitdfrac" { MathStyle::Display } else { MathStyle::Text };
                nodes.push(Node::Frac(Box::new(Frac { num, den, bar: Some(0.0), style: Some(style), left: '\0', right: '\0', continued: true, align: (-1, 1), bevelled: false })));
                Ok(true)
            }
            "xmathstrut" => {
                let depth = self.get_brackets(name)?;
                let height = self.get_arg(name)?;
                let number = |raw: &str| raw.trim().parse::<f32>().map_err(|_| format!("Argument to {name} must be a number"));
                let up = number(&height)?;
                let down = depth.as_deref().map_or(Ok(up), number)?;
                nodes.push(Node::Rule { width: 0.0, height: 0.75 * (1.0 + up) + 0.25 * (1.0 + down), raise: -0.25 * (1.0 + down) });
                Ok(true)
            }
            "prescript" => {
                let sup = self.parse_arg(name)?;
                let sub = self.parse_arg(name)?;
                let base = self.parse_arg(name)?;
                nodes.push(if sup.is_empty() && sub.is_empty() { Node::Group(base) } else { Node::Prescript { base, sup, sub } });
                Ok(true)
            }
            "newtagform" | "renewtagform" => {
                let form = self.get_arg(name)?.trim().to_owned();
                let inner = self.get_brackets(name)?.unwrap_or_default();
                let (left, right) = (self.get_arg(name)?, self.get_arg(name)?);
                let known = self.defs.tag_forms.contains_key(&form);
                if name == "newtagform" && known {
                    return Err(format!("Duplicate definition of tag form '{form}'"));
                }
                if name == "renewtagform" && !known {
                    return Err(format!("Undefined tag form '{form}'"));
                }
                self.defs.tag_forms.insert(form, (inner, left, right));
                Ok(true)
            }
            "usetagform" => {
                let form = self.get_arg(name)?.trim().to_owned();
                if form == "default" {
                    self.defs.tag_form = None;
                } else {
                    let found = self.defs.tag_forms.get(&form).cloned().ok_or_else(|| format!("Undefined tag form '{form}'"))?;
                    self.defs.tag_form = Some(found);
                }
                Ok(true)
            }
            "adjustlimits" => {
                let first = self.get_arg(name)?;
                self.skip_space();
                if !self.eat('_') {
                    return Err(format!("Use of \\{name} doesn't match its definition"));
                }
                let first_sub = self.get_arg(name)?;
                let second = self.get_arg(name)?;
                self.skip_space();
                if !self.eat('_') {
                    return Err(format!("Use of \\{name} doesn't match its definition"));
                }
                let second_sub = self.get_arg(name)?;
                self.splice(&format!("\\mathop{{{{{first}}}\\vphantom{{{{{second}}}}}}}_{{{{{first_sub}}}\\vphantom{{{{{second_sub}}}}}}}\\mathop{{{{{second}}}\\vphantom{{{{{first}}}}}}}_{{{{{second_sub}}}\\vphantom{{{{{first_sub}}}}}}}"))?;
                Ok(true)
            }
            "mathtoolsset" => {
                self.get_arg(name)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn relation(&mut self, tex: &str) -> Parsed<bool> {
        let body = tex.replace(':', "\\MTThinColon ").replace('-', "\\mathrel{-}");
        self.splice(&format!("\\mathrel{{{body}}}"))?;
        Ok(true)
    }

    /// `\ref`: what the label's formula was tagged, or what MathJax shows for one with no tag.
    pub(super) fn reference(&self, label: &str, equation: bool) -> Node {
        let tag = self.defs.labels.get(label.trim()).map_or("###", String::as_str);
        Node::Text { text: if equation { format!("({tag})") } else { tag.to_owned() }, font: MathFont::Roman }
    }

    /// What `\tag{...}` shows: its parentheses, or the form `\usetagform` chose.
    pub(super) fn tag_shown(&mut self, body: Vec<Node>) -> Parsed<Vec<Node>> {
        let Some((inner, left, right)) = self.defs.tag_form.clone() else {
            let mut shown = vec![Node::Text { text: "(".to_owned(), font: MathFont::Roman }];
            shown.extend(body);
            shown.push(Node::Text { text: ")".to_owned(), font: MathFont::Roman });
            return Ok(shown);
        };
        let mut shown = self.parse_text(&left, MathFont::Roman)?;
        if inner.is_empty() {
            shown.extend(body);
        } else {
            // `[\textbf]`: the command takes the tag as its argument, which is already drawn by now.
            let wrapped = self.parse_string(&format!("{inner}{{}}"))?;
            shown.extend(wrapped);
            shown.extend(body);
        }
        shown.extend(self.parse_text(&right, MathFont::Roman)?);
        Ok(shown)
    }

    // ------------------------------------------------------------ enclose, cancel, bbox, extpfeil, centernot

    /// `ParseUtil.keyvalOptions` with `ENCLOSE_OPTIONS`.
    fn enclose_options(&mut self, raw: &str) -> Parsed<Enclose> {
        let mut out = Enclose { notations: Vec::new(), color: None, fill: None, padding: None, thickness: None, arrowhead: (1.0, 1.0, 1.0), body: Vec::new() };
        for part in split_top(raw, ',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            let (key, value) = (key.trim(), value.trim().trim_matches(|c| c == '"' || c == '\''));
            match key {
                "color" | "mathcolor" => out.color = self.color(None, value),
                "background" | "mathbackground" => out.fill = self.color(None, value),
                "data-padding" => out.padding = dimension(value),
                "data-thickness" => out.thickness = dimension(value),
                "data-arrowhead" => {
                    for piece in value.split(';') {
                        let Some((name, number)) = piece.split_once(':') else { continue };
                        let Ok(number) = number.trim().parse::<f32>() else { continue };
                        match name.trim() {
                            "x" => out.arrowhead.0 = number,
                            "y" => out.arrowhead.1 = number,
                            "dx" => out.arrowhead.2 = number,
                            _ => {}
                        }
                    }
                }
                _ => return Err(format!("Invalid option: {key}")),
            }
        }
        Ok(out)
    }

    fn drawn(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        match name {
            "enclose" => {
                let notation = self.get_arg(name)?;
                let options = self.get_brackets(name)?.unwrap_or_default();
                let mut enclose = self.enclose_options(&options)?;
                enclose.notations = notation.replace(',', " ").split_whitespace().filter_map(notation_named).collect();
                enclose.body = self.parse_arg(name)?;
                nodes.push(Node::Enclose(Box::new(enclose)));
                Ok(true)
            }
            "cancel" | "bcancel" | "xcancel" => {
                let options = self.get_brackets(name)?.unwrap_or_default();
                let mut enclose = self.enclose_options(&options)?;
                enclose.notations = match name {
                    "cancel" => vec![Notation::UpDiagonalStrike],
                    "bcancel" => vec![Notation::DownDiagonalStrike],
                    _ => vec![Notation::UpDiagonalStrike, Notation::DownDiagonalStrike],
                };
                enclose.body = self.parse_arg(name)?;
                nodes.push(Node::Enclose(Box::new(enclose)));
                Ok(true)
            }
            "cancelto" => {
                let options = self.get_brackets(name)?.unwrap_or_default();
                let mut enclose = self.enclose_options(&options)?;
                let value = self.parse_arg(name)?;
                enclose.notations = vec![Notation::UpDiagonalStrike, Notation::UpDiagonalArrow, Notation::NorthEastArrow];
                enclose.body = self.parse_arg(name)?;
                let sup = Node::Raise { by: 0.1, body: value };
                nodes.push(Node::Scripts { base: Box::new(Node::Enclose(Box::new(enclose))), sup: Some(Box::new(sup)), sub: None, primes: 0 });
                Ok(true)
            }
            "framebox" => {
                let width = self.get_brackets(name)?.and_then(|raw| dimension(&raw));
                let align = match self.get_brackets(name)?.as_deref().map(str::trim) {
                    Some("l") => -1,
                    Some("r") => 1,
                    _ => 0,
                };
                let raw = self.get_arg(name)?;
                let body = self.parse_text(&raw, MathFont::Roman)?;
                let body = if width.is_some() { vec![Node::Sized { width, align, body }] } else { body };
                nodes.push(Node::Enclose(Box::new(Enclose { notations: vec![Notation::Box], color: None, fill: None, padding: None, thickness: None, arrowhead: (1.0, 1.0, 1.0), body })));
                Ok(true)
            }
            "makebox" => {
                let width = self.get_brackets(name)?.and_then(|raw| dimension(&raw));
                let align = position(self.get_brackets(name)?.as_deref());
                let raw = self.get_arg(name)?;
                let body = self.parse_text(&raw, MathFont::Roman)?;
                nodes.push(Node::Sized { width, align, body });
                Ok(true)
            }
            "bbox" => {
                let options = self.get_brackets(name)?.unwrap_or_default();
                let body = self.parse_arg(name)?;
                nodes.push(Node::BBox(Box::new(self.bbox_options(&options, name, body)?)));
                Ok(true)
            }
            "Newextarrow" => {
                let cs = self.get_arg(name)?;
                let Some(target) = cs.strip_prefix('\\').filter(|rest| rest.chars().count() == 1 || (!rest.is_empty() && rest.chars().all(is_letter))).map(str::to_owned) else {
                    return Err(format!("First argument to \\{name} must be a control sequence name"));
                };
                let space = self.get_arg(name)?;
                let code = self.get_arg(name)?;
                let pad = space.split_once(',').and_then(|(l, r)| Some((l.trim().parse::<f32>().ok()?, r.trim().parse::<f32>().ok()?))).ok_or_else(|| format!("Second argument to \\{name} must be two integers separated by a comma"))?;
                let code = code.trim();
                let number = code.strip_prefix("0x").or_else(|| code.strip_prefix("0X")).map_or_else(|| code.parse::<u32>().ok(), |hex| u32::from_str_radix(hex, 16).ok());
                let glyph = number.and_then(char::from_u32).ok_or_else(|| format!("Third argument to \\{name} must be a unicode character number"))?;
                self.define(target, Macro::Arrow { glyph, pad });
                Ok(true)
            }
            "centerOver" => {
                let base = self.parse_arg(name)?;
                let over = self.parse_arg(name)?;
                nodes.push(Node::CenterOver { base, over });
                Ok(true)
            }
            "centernot" => {
                let base = self.parse_arg(name)?;
                nodes.push(Node::CenterOver { base, over: vec![Node::Sym { text: "⧸".to_owned(), class: Class::Ord, font: MathFont::Roman, bold: false }] });
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(super) fn x_arrow_node(&mut self, name: &str, arrow: XArrow) -> Parsed<Node> {
        let below = self.get_brackets(name)?.unwrap_or_default();
        let above = self.parse_arg(name)?;
        let below = self.parse_string(&below)?;
        Ok(Node::Arrow { arrow, above, below })
    }

    /// `\bbox[5px,#fee,border:1px solid red]`. Past what `BBoxStyle` hands on, what its SVG output can read of a style is its border
    /// and its padding.
    fn bbox_options(&mut self, raw: &str, name: &str, body: Vec<Node>) -> Parsed<BBox> {
        let mut out = BBox { padding: [0.0; 4], fill: None, borders: [None; 4], body };
        let (mut seen_pad, mut seen_fill, mut seen_style) = (false, false, false);
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let number_end = part.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(part.len());
            let is_dimension = number_end > 0 && matches!(&part[number_end..], "pt" | "em" | "ex" | "mu" | "px" | "in" | "cm" | "mm");
            if is_dimension {
                if std::mem::replace(&mut seen_pad, true) {
                    return Err(format!("Padding specified twice in \\{name}"));
                }
                let pad = dimension(part).unwrap_or(0.0);
                out.padding = [pad; 4];
            } else if part.bytes().all(|b| b.is_ascii_alphanumeric()) || (part.starts_with('#') && matches!(part.len(), 4 | 7) && part[1..].bytes().all(|b| b.is_ascii_hexdigit())) {
                if std::mem::replace(&mut seen_fill, true) {
                    return Err(format!("Background specified twice in \\{name}"));
                }
                out.fill = self.color(None, part);
            } else if part.split(':').next().is_some_and(|key| !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')) && part.contains(':') {
                if std::mem::replace(&mut seen_style, true) {
                    return Err(format!("Style specified twice in \\{name}"));
                }
                self.bbox_style(part, &mut out);
            } else {
                return Err(format!("\"{part}\" doesn't look like a color, a padding dimension, or a style"));
            }
        }
        Ok(out)
    }

    fn bbox_style(&mut self, style: &str, out: &mut BBox) {
        for declaration in style.split(';') {
            let Some((key, value)) = declaration.split_once(':') else { continue };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            let side = |key: &str, prefix: &str| -> Option<Vec<usize>> {
                Some(match key.strip_prefix(prefix)? {
                    "" => vec![0, 1, 2, 3],
                    "-top" => vec![0],
                    "-right" => vec![1],
                    "-bottom" => vec![2],
                    "-left" => vec![3],
                    _ => return None,
                })
            };
            if let Some(sides) = side(&key, "border") {
                let mut border = Border { width: 0.1, style: LineStyle::Solid, color: None };
                for word in value.split_whitespace() {
                    match word {
                        "solid" => border.style = LineStyle::Solid,
                        "dashed" => border.style = LineStyle::Dashed,
                        "dotted" => border.style = LineStyle::Dotted,
                        "double" => border.style = LineStyle::Double,
                        "none" | "hidden" => border.width = 0.0,
                        "thin" => border.width = 0.1,
                        "medium" => border.width = 0.2,
                        "thick" => border.width = 0.3,
                        word => match dimension(word) {
                            Some(width) => border.width = width,
                            None => border.color = self.color(None, word),
                        },
                    }
                }
                for at in sides {
                    out.borders[at] = Some(border);
                }
            } else if let Some(sides) = side(&key, "padding") {
                let widths: Vec<f32> = value.split_whitespace().filter_map(dimension).collect();
                for at in sides {
                    if let Some(width) = widths.first() {
                        out.padding[at] = *width;
                    }
                }
            } else if key == "background" || key == "background-color" {
                out.fill = self.color(None, value);
            }
        }
    }

    // ------------------------------------------------------------ empheq, colortbl

    fn empheq_delimiter(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let Some(rest) = name.strip_prefix("empheq") else { return Ok(false) };
        let rest = rest.strip_prefix("big").unwrap_or(rest);
        let delim = match rest {
            "l" | "r" => self.delimiter(name)?,
            "lbrace" => '{',
            "rbrace" => '}',
            "lbrack" => '[',
            "rbrack" => ']',
            "langle" => '⟨',
            "rangle" => '⟩',
            "lparen" => '(',
            "rparen" => ')',
            "lvert" | "rvert" => '|',
            "lVert" | "rVert" => '\u{2016}',
            "lfloor" => '⌊',
            "rfloor" => '⌋',
            "lceil" => '⌈',
            "rceil" => '⌉',
            _ => return Ok(false),
        };
        nodes.push(Node::Stretch(delim));
        Ok(true)
    }

    /// `\begin{empheq}[left=...,right=...]{align}`.
    pub(super) fn empheq(&mut self) -> Parsed<Node> {
        let options = self.get_brackets("begin{empheq}")?.unwrap_or_default();
        let inner = self.get_arg("begin{empheq}")?;
        let (env, count) = match inner.split_once('=') {
            Some((env, count)) => (env.trim().to_owned(), Some(count.trim().to_owned())),
            None => (inner.trim().to_owned(), None),
        };
        if !matches!(env.as_str(), "equation" | "equation*" | "align" | "align*" | "gather" | "gather*" | "multline" | "multline*" | "alignat" | "alignat*" | "xalignat" | "xalignat*" | "xxalignat" | "flalign" | "flalign*" | "cases") {
            return Err(format!("Invalid environment \"{env}\" for \\begin{{empheq}}"));
        }
        let (mut left, mut right) = (String::new(), String::new());
        for part in split_top(&options, ',') {
            let Some((key, value)) = part.split_once('=') else {
                if part.trim().is_empty() {
                    continue;
                }
                return Err(format!("Invalid option: {}", part.trim()));
            };
            let value = value.trim();
            let value = value.strip_prefix('{').and_then(|v| v.strip_suffix('}')).unwrap_or(value);
            match key.trim() {
                "left" => left = value.to_owned(),
                "right" => right = value.to_owned(),
                other => return Err(format!("Invalid option: {other}")),
            }
        }
        if let Some(count) = count {
            self.splice(&format!("{{{count}}}"))?;
        }
        let saved_text = (self.text.clone(), self.at);
        // What it holds ends at `\end{empheq}`, which is the inner environment's end as well.
        let rest = &saved_text.0[saved_text.1..];
        let close = rest.find("\\end{empheq}").ok_or_else(|| "Missing \\end{empheq}".to_owned())?;
        let body_raw = format!("\\begin{{{env}}}{}\\end{{{env}}}", &rest[..close]);
        self.at += close + "\\end{empheq}".len();
        let body = self.parse_inner_environment(&body_raw)?;
        let left = self.parse_string(&left)?;
        let right = self.parse_string(&right)?;
        Ok(Node::Empheq { left, right, body })
    }

    /// An equation environment that is the first thing in a nest of its own, as `empheq` and `spreadlines` allow.
    fn parse_inner_environment(&mut self, raw: &str) -> Parsed<Vec<Node>> {
        let saved = std::mem::replace(&mut self.nesting, true);
        let result = self.parse_string(raw);
        self.nesting = saved;
        result
    }

    fn colortbl(&mut self, name: &str) -> Parsed<bool> {
        if !matches!(name, "cellcolor" | "rowcolor" | "columncolor") {
            return Ok(false);
        }
        let model = self.get_brackets(name)?;
        let spec = self.get_arg(name)?;
        let color = self.color(model.as_deref(), &spec);
        let Some(cell) = &mut self.cell else { return Err(format!("Unsupported use of \\{name}")) };
        match name {
            "cellcolor" => cell.fill = color,
            "rowcolor" => cell.row_fill = color,
            _ => {
                if !cell.first_row {
                    return Err(format!("\\{name} must be in the top row or preamble"));
                }
                if let Some(color) = color {
                    cell.column_fill = Some((cell.column, color));
                }
                self.get_brackets(name)?;
                self.get_brackets(name)?;
            }
        }
        Ok(true)
    }

    // ------------------------------------------------------------ bussproofs

    pub(super) fn prooftree(&mut self) -> Parsed<Node> {
        let saved = self.proof.replace(ProofState { stack: Vec::new(), line: ProofLine::Solid, current: ProofLine::Solid, root_at_top: false, left: None, right: None });
        let result = self.prooftree_inner();
        let state = std::mem::replace(&mut self.proof, saved);
        result?;
        let mut state = state.expect("set above");
        if state.stack.len() != 1 {
            return Err("Proof tree badly specified.".to_owned());
        }
        Ok(Node::Proof(Box::new(state.stack.pop().expect("one"))))
    }

    fn prooftree_inner(&mut self) -> Parsed<()> {
        loop {
            let (nodes, stop) = self.list()?;
            if !nodes.is_empty() && !nodes.iter().all(|n| matches!(n, Node::Group(g) if g.is_empty())) {
                return Err("Proof tree badly specified.".to_owned());
            }
            match stop {
                Stop::End(found) if found.trim() == "prooftree" => return Ok(()),
                Stop::End(found) => return Err(format!("\\begin{{prooftree}} ended with \\end{{{found}}}")),
                Stop::Eof => return Err("Missing \\end{prooftree}".to_owned()),
                _ => return Err("Proof tree badly specified.".to_owned()),
            }
        }
    }

    /// `paddedContent`: text, half an `ex` clear of the rule on either side.
    fn proof_content(&mut self, raw: &str) -> Parsed<Vec<Node>> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = vec![Node::Space(0.215)];
        out.extend(self.parse_text(raw, MathFont::Roman)?);
        out.push(Node::Space(0.215));
        Ok(out)
    }

    /// `parseFCenterLine`: `$A \fCenter B$`.
    fn proof_sequent(&mut self, name: &str) -> Parsed<(Vec<Node>, Vec<Node>)> {
        self.skip_space();
        if !self.eat('$') {
            return Err(format!("Use of \\{name} does not match its definition."));
        }
        let axiom = self.get_up_to(name, '$')?;
        let Some((premise, conclusion)) = axiom.split_once("\\fCenter") else { return Err(format!("Missing \\fCenter in \\{name}.")) };
        Ok((self.parse_string(premise)?, self.parse_string(conclusion)?))
    }

    fn proof_command(&mut self, name: &str) -> Parsed<bool> {
        let known = matches!(name, "AxiomC" | "AXC" | "UnaryInfC" | "UIC" | "BinaryInfC" | "BIC" | "TrinaryInfC" | "TIC" | "QuaternaryInfC" | "QuinaryInfC" | "RightLabel" | "RL" | "LeftLabel" | "LL" | "noLine" | "singleLine" | "solidLine" | "dashedLine" | "alwaysNoLine" | "alwaysSingleLine" | "alwaysSolidLine" | "alwaysDashedLine" | "rootAtTop" | "alwaysRootAtTop" | "rootAtBottom" | "alwaysRootAtBottom" | "fCenter" | "Axiom" | "UnaryInf" | "BinaryInf" | "TrinaryInf" | "QuaternaryInf" | "QuinaryInf");
        if !known {
            return Ok(false);
        }
        if name == "fCenter" {
            return Ok(true);
        }
        if self.proof.is_none() {
            return Err("Proof commands only allowed in prooftree environment.".to_owned());
        }
        match name {
            "AxiomC" | "AXC" => {
                let raw = self.get_arg(name)?;
                let conclusion = self.proof_content(&raw)?;
                let proof = Proof { premises: Vec::new(), conclusion, centered: None, left: None, right: None, line: ProofLine::None, root_at_top: false, inference: false };
                self.proof_state().stack.push(proof);
            }
            "Axiom" => {
                let (conclusion, right) = self.proof_sequent(name)?;
                let proof = Proof { premises: Vec::new(), conclusion, centered: Some(right), left: None, right: None, line: ProofLine::None, root_at_top: false, inference: false };
                self.proof_state().stack.push(proof);
            }
            "UnaryInfC" | "UIC" | "BinaryInfC" | "BIC" | "TrinaryInfC" | "TIC" | "QuaternaryInfC" | "QuinaryInfC" | "UnaryInf" | "BinaryInf" | "TrinaryInf" | "QuaternaryInf" | "QuinaryInf" => {
                let count = match name {
                    "UnaryInfC" | "UIC" | "UnaryInf" => 1,
                    "BinaryInfC" | "BIC" | "BinaryInf" => 2,
                    "TrinaryInfC" | "TIC" | "TrinaryInf" => 3,
                    "QuaternaryInfC" | "QuaternaryInf" => 4,
                    _ => 5,
                };
                if self.proof_state().stack.len() < count {
                    return Err("Proof tree badly specified.".to_owned());
                }
                let (conclusion, centered) = if name.ends_with('C') || matches!(name, "UIC" | "BIC" | "TIC") {
                    let raw = self.get_arg(name)?;
                    (self.proof_content(&raw)?, None)
                } else {
                    let (left, right) = self.proof_sequent(name)?;
                    (left, Some(right))
                };
                let state = self.proof_state();
                let at = state.stack.len() - count;
                let premises = state.stack.split_off(at);
                let line = state.current;
                state.current = state.line;
                let (left, right) = (state.left.take(), state.right.take());
                let root_at_top = state.root_at_top;
                state.stack.push(Proof { premises, conclusion, centered, left, right, line, root_at_top, inference: true });
            }
            "RightLabel" | "RL" | "LeftLabel" | "LL" => {
                let raw = self.get_arg(name)?;
                let label = self.parse_text(&raw, MathFont::Roman)?;
                let state = self.proof_state();
                if matches!(name, "RightLabel" | "RL") {
                    state.right = Some(label);
                } else {
                    state.left = Some(label);
                }
            }
            "noLine" | "singleLine" | "solidLine" | "dashedLine" | "alwaysNoLine" | "alwaysSingleLine" | "alwaysSolidLine" | "alwaysDashedLine" => {
                let line = if name.ends_with("noLine") || name.ends_with("NoLine") {
                    ProofLine::None
                } else if name.ends_with("ashedLine") {
                    ProofLine::Dashed
                } else {
                    ProofLine::Solid
                };
                let state = self.proof_state();
                state.current = line;
                if name.starts_with("always") {
                    state.line = line;
                }
            }
            "rootAtTop" | "alwaysRootAtTop" => self.proof_state().root_at_top = true,
            _ => self.proof_state().root_at_top = false,
        }
        Ok(true)
    }

    fn proof_state(&mut self) -> &mut ProofState {
        self.proof.as_mut().expect("checked by the caller")
    }

    // ------------------------------------------------------------ amscd

    /// The arrow an `@` has just opened, from the character after it, in a `\begin{CD}`.
    pub(super) fn cd_arrow(&mut self, kind: char) -> Parsed<Vec<Node>> {
        if kind == '.' {
            return Ok(Vec::new());
        }
        let (mut above, mut below) = (Vec::new(), Vec::new());
        if matches!(kind, '>' | '<' | 'V' | 'A') {
            let name = format!("@{kind}");
            let first = self.get_up_to(&name, kind)?;
            let second = self.get_up_to(&name, kind)?;
            above = self.parse_string(&first)?;
            below = self.parse_string(&second)?;
        }
        Ok(vec![Node::CdArrow(Box::new(CdArrow { kind, above, below, least: self.diagram.unwrap_or((2.75, 1.75)) }))])
    }

    // ------------------------------------------------------------ braket

    fn braket(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let tex = match name {
            "Bra" => format!("{{\\left\\langle {{{}}} \\right\\vert}}", self.get_arg(name)?),
            "Ket" => format!("{{\\left\\vert {{{}}} \\right\\rangle}}", self.get_arg(name)?),
            "Ketbra" => {
                let (ket, bra) = (self.get_arg(name)?, self.get_arg(name)?);
                format!("{{\\left\\vert {{{ket}}} \\right\\rangle\\left\\langle {{{bra}}} \\right\\vert}}")
            }
            "set" => {
                let arg = self.get_arg(name)?;
                format!("\\mathopen{{\\{{}}{arg}\\mathclose{{\\}}}}")
            }
            "Braket" | "Set" => {
                let arg = self.get_arg(name)?;
                let most = if name == "Set" { 1 } else { usize::MAX };
                let inner = stretch_bars(&arg, most);
                if name == "Set" {
                    format!("\\left\\{{\\,{inner}\\,\\right\\}}")
                } else {
                    format!("\\left\\langle {inner}\\right\\rangle")
                }
            }
            _ => return Ok(false),
        };
        self.parsed(&tex, nodes)
    }

    // ------------------------------------------------------------ what is left of base and ams

    /// `BaseMethods.Matrix`: the plain TeX forms, whose rows end in `\cr` and whose array is a brace.
    fn plain_matrix(&mut self, name: &str, shell: Array) -> Parsed<Node> {
        let raw = self.get_arg(name)?;
        let outer = std::mem::replace(&mut self.text, raw);
        let at = std::mem::replace(&mut self.at, 0);
        let array = self.array_rows(shell, None, &CellWrap::default());
        self.text = outer;
        self.at = at;
        Ok(Node::Array(Box::new(array?)))
    }

    fn base_extras(&mut self, name: &str, nodes: &mut Vec<Node>) -> Parsed<bool> {
        let base = plain_array;
        let display = |columns, col_gap| Array { columns, col_gap, style: MathStyle::Display, jot: 0.5, ..plain_array() };
        let node = match name {
            "*" => return Ok(true),
            "matrix" | "array" => self.plain_matrix(name, base())?,
            "pmatrix" => self.plain_matrix(name, Array { left: '(', right: ')', ..base() })?,
            "cases" => self.plain_matrix(name, Array { columns: vec!['l'], left: '{', ..base() })?,
            "eqalign" => self.plain_matrix(name, Array { paired: true, ..display(vec!['r', 'l'], 0.0) })?,
            "displaylines" => self.plain_matrix(name, display(vec!['c'], 0.0))?,
            "eqalignno" | "leqalignno" => self.plain_matrix(name, Array { paired: true, ..display(vec!['r', 'l'], 0.0) })?,
            "hline" | "hdashline" => return Err(format!("Misplaced \\{name}")),
            "mathchoice" => {
                let parts = [self.parse_arg(name)?, self.parse_arg(name)?, self.parse_arg(name)?, self.parse_arg(name)?];
                Node::Choice(Box::new(parts))
            }
            "vcenter" | "vtop" | "vbox" => {
                let align = match name {
                    "vcenter" => VAlign::Center,
                    "vtop" => VAlign::Top,
                    _ => VAlign::Bottom,
                };
                let saved = self.hsize.take();
                let parsed = self.parse_arg(name);
                let width = std::mem::replace(&mut self.hsize, saved);
                let body = parsed?;
                let body = if width.is_some() { vec![Node::Sized { width, align: -1, body }] } else { body };
                Node::VBox { align, body }
            }
            "hsize" => {
                self.skip_space();
                self.eat('=');
                self.hsize = Some(self.dimension_here(name)?);
                return Ok(true);
            }
            "parbox" => {
                let align = match self.get_brackets(name)?.as_deref().map(str::trim) {
                    Some("t") => VAlign::Top,
                    Some("b") => VAlign::Bottom,
                    _ => VAlign::Center,
                };
                let width = self.dimension_arg(name)?;
                let raw = self.get_arg(name)?;
                let body = self.parse_text(&raw, MathFont::Roman)?;
                Node::VBox { align, body: vec![Node::Sized { width: Some(width), align: -1, body }] }
            }
            "breakAlign" => {
                if self.cell.is_none() {
                    return Err(format!("\\{name} must be used in an alignment environment"));
                }
                let kind = self.get_arg(name)?;
                if !matches!(kind.trim(), "c" | "r" | "t") {
                    return Err(format!("First argument to \\{name} must be one of c, r, or t"));
                }
                self.get_arg(name)?;
                return Ok(true);
            }
            "ref" | "eqref" => {
                let label = self.get_arg(name)?;
                self.reference(&label, name == "eqref")
            }
            "mmlToken" => self.mml_token(name)?,
            "idotsint" => {
                self.splice("\\int\\cdots\\int ")?;
                return Ok(true);
            }
            "minCDarrowwidth" => {
                self.cd_least.0 = self.dimension_arg(name)?;
                return Ok(true);
            }
            "minCDarrowheight" => {
                self.cd_least.1 = self.dimension_arg(name)?;
                return Ok(true);
            }
            "sideset" => self.sideset(name)?,
            _ => return Ok(false),
        };
        nodes.push(node);
        Ok(true)
    }

    /// `AmsMethods.SideSet`: scripts for the corners, read off the two arguments' own scripts.
    fn sideset(&mut self, name: &str) -> Parsed<Node> {
        let pre = self.parse_arg(name)?;
        let post = self.parse_arg(name)?;
        let base = self.parse_arg(name)?;
        let split = |nodes: Vec<Node>| -> (Vec<Node>, Vec<Node>) {
            let (mut sup, mut sub) = (Vec::new(), Vec::new());
            for node in nodes {
                if let Node::Scripts { sup: up, sub: down, .. } = node {
                    sup.extend(up.map(|b| *b));
                    sub.extend(down.map(|b| *b));
                }
            }
            (sup, sub)
        };
        let (pre_sup, pre_sub) = split(pre);
        let (post_sup, post_sub) = split(post);
        let base = if base.len() == 1 { base.into_iter().next().expect("one") } else { Node::Group(base) };
        Ok(Node::SideSet { pre_sup, pre_sub, post_sup, post_sub, base: Box::new(base) })
    }

    fn mml_token(&mut self, name: &str) -> Parsed<Node> {
        let kind = self.get_arg(name)?;
        let kind = kind.trim();
        if !matches!(kind, "mi" | "mn" | "mo" | "mtext" | "ms") {
            return Err(format!("{kind} is not a token element"));
        }
        let options = self.get_brackets(name)?.unwrap_or_default();
        let text = self.get_arg(name)?;
        let mut class = if kind == "mo" { unicode_class(single_char(&text).unwrap_or('\0')).unwrap_or(Class::Rel) } else { Class::Ord };
        let mut font = if kind == "mi" && text.chars().count() == 1 { MathFont::Auto } else { MathFont::Roman };
        let mut color = None;
        for part in split_top(&options, ',') {
            let Some((key, value)) = part.split_once('=') else { continue };
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            match key.trim() {
                "texClass" => {
                    class = match value {
                        "ORD" => Class::Ord,
                        "OP" => Class::Op,
                        "BIN" => Class::Bin,
                        "REL" => Class::Rel,
                        "OPEN" => Class::Open,
                        "CLOSE" => Class::Close,
                        "PUNCT" => Class::Punct,
                        "INNER" => Class::Inner,
                        _ => class,
                    };
                }
                "mathvariant" => {
                    font = match value {
                        "normal" => MathFont::Roman,
                        "bold" => MathFont::Bold,
                        "italic" => MathFont::Italic,
                        "bold-italic" => MathFont::BoldItalic,
                        "double-struck" => MathFont::DoubleStruck,
                        "script" => MathFont::Script,
                        "fraktur" => MathFont::Fraktur,
                        "sans-serif" => MathFont::Sans,
                        "monospace" => MathFont::Mono,
                        _ => font,
                    };
                }
                "mathcolor" | "color" => color = self.color(None, value),
                _ => {}
            }
        }
        let sym = if kind == "mtext" || kind == "ms" { Node::Text { text, font } } else { Node::Sym { text, class, font, bold: false } };
        Ok(match color {
            Some(color) => Node::Colored { color, body: vec![sym] },
            None => sym,
        })
    }
}

fn position(raw: Option<&str>) -> i8 {
    match raw.map(|p| p.trim().to_ascii_lowercase()).as_deref() {
        Some("r") => 1,
        Some("l") => -1,
        // `s` spreads a line across its width, which a box of one line does not have room to show.
        _ => 0,
    }
}

/// `\def`'s parameter text: the delimiters, or plain arguments when there are none.
fn def_template(template: &str, body: String) -> Option<Macro> {
    let mut before = vec![String::new()];
    let mut next = 1;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            let digit = chars.next()?.to_digit(10)?;
            if digit != next {
                return None;
            }
            next += 1;
            before.push(String::new());
        } else if let Some(last) = before.last_mut() {
            last.push(c);
        }
    }
    if before.iter().all(|d| d.trim().is_empty()) {
        return Some(Macro::Text { args: (next - 1) as u8, opt: None, body });
    }
    Some(Macro::Template { before, body })
}

fn split_top(raw: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0, 0);
    let mut quote = None;
    for (at, c) in raw.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') => depth -= 1,
            (None, c) if c == separator && depth == 0 => {
                parts.push(&raw[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&raw[start..]);
    parts
}

fn split_at_amp(raw: &str) -> Option<(&str, &str)> {
    let mut depth = 0;
    let mut escaped = false;
    for (at, c) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => depth -= 1,
            '&' if depth == 0 => return Some((&raw[..at], &raw[at + 1..])),
            _ => {}
        }
    }
    None
}

/// `\Braket`'s bars, at the top of its text, up to as many as it takes.
fn stretch_bars(raw: &str, most: usize) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    let (mut depth, mut count) = (0, 0);
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(next) = chars.next() else {
                    out.push('\\');
                    break;
                };
                if next == '|' && depth == 0 && count < most {
                    count += 1;
                    out.push_str("\\middle\\|");
                } else {
                    out.push('\\');
                    out.push(next);
                }
            }
            '{' => {
                depth += 1;
                out.push(c);
            }
            '}' => {
                depth -= 1;
                out.push(c);
            }
            '|' if depth == 0 && count < most => {
                count += 1;
                if chars.peek() == Some(&'|') {
                    chars.next();
                    out.push_str("\\middle\\|");
                } else {
                    out.push_str("\\middle|");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// `outputBraket`.
fn bracket_text([a, b, c]: [&str; 3], star1: bool, star2: bool) -> String {
    if star1 && star2 {
        format!("\\left\\langle{{{a}}}\\middle\\vert{{{b}}}\\middle\\vert{{{c}}}\\right\\rangle")
    } else if star1 {
        format!("\\langle{{{a}}}\\vert{{{b}}}\\vert{{{c}}}\\rangle")
    } else {
        format!("\\left\\langle{{{a}}}\\right\\vert{{{b}}}\\left\\vert{{{c}}}\\right\\rangle")
    }
}

fn notation_named(name: &str) -> Option<Notation> {
    Some(match name {
        "longdiv" => Notation::LongDiv,
        "actuarial" => Notation::Actuarial,
        "radical" => Notation::Radical,
        "box" => Notation::Box,
        "roundedbox" => Notation::RoundedBox,
        "circle" => Notation::Circle,
        "left" => Notation::Left,
        "right" => Notation::Right,
        "top" => Notation::Top,
        "bottom" => Notation::Bottom,
        "updiagonalstrike" => Notation::UpDiagonalStrike,
        "downdiagonalstrike" => Notation::DownDiagonalStrike,
        "verticalstrike" => Notation::VerticalStrike,
        "horizontalstrike" => Notation::HorizontalStrike,
        "updiagonalarrow" => Notation::UpDiagonalArrow,
        "northeastarrow" => Notation::NorthEastArrow,
        "southeastarrow" => Notation::SouthEastArrow,
        "southwestarrow" => Notation::SouthWestArrow,
        "northwestarrow" => Notation::NorthWestArrow,
        "uparrow" => Notation::UpArrow,
        "downarrow" => Notation::DownArrow,
        "leftarrow" => Notation::LeftArrow,
        "rightarrow" => Notation::RightArrow,
        "updownarrow" => Notation::UpDownArrow,
        "leftrightarrow" => Notation::LeftRightArrow,
        "phasorangle" => Notation::PhasorAngle,
        "madruwb" => Notation::Madruwb,
        _ => return None,
    })
}

/// What a command in `\text` is when it is simply these characters.
pub(super) fn text_literal(name: &str) -> Option<&'static str> {
    Some(match name {
        "$" => "$",
        "_" => "_",
        "%" => "%",
        "{" => "{",
        "}" => "}",
        " " => " ",
        "&" => "&",
        "#" => "#",
        "dagger" => "\u{2020}",
        "ddagger" => "\u{2021}",
        "S" => "\u{a7}",
        "AA" => "\u{212b}",
        "ldots" => "\u{2026}",
        "vdots" => "\u{22ee}",
        _ => return None,
    })
}

/// `\'e` and its kind in text: the mark to put over what follows.
pub(super) fn text_accent(name: &str) -> Option<char> {
    Some(match name {
        "'" | "\u{2019}" => '\u{301}',
        "`" | "\u{2018}" => '\u{300}',
        "^" => '\u{302}',
        "\"" => '\u{308}',
        "~" => '\u{303}',
        "=" => '\u{304}',
        "." => '\u{307}',
        "u" => '\u{306}',
        "v" => '\u{30c}',
        _ => return None,
    })
}

/// `textcomp`: the same in math and in text.
pub(super) fn text_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "textasciicircum" => "^",
        "textasciitilde" => "~",
        "textasteriskcentered" => "*",
        "textbackslash" => "\\",
        "textbar" => "|",
        "textbraceleft" => "{",
        "textbraceright" => "}",
        "textbullet" => "\u{2022}",
        "textdagger" => "\u{2020}",
        "textdaggerdbl" => "\u{2021}",
        "textellipsis" => "\u{2026}",
        "textemdash" => "\u{2014}",
        "textendash" => "\u{2013}",
        "textexclamdown" => "\u{a1}",
        "textgreater" => ">",
        "textless" => "<",
        "textordfeminine" => "\u{aa}",
        "textordmasculine" => "\u{ba}",
        "textparagraph" => "\u{b6}",
        "textperiodcentered" => "\u{b7}",
        "textquestiondown" => "\u{bf}",
        "textquotedblleft" => "\u{201c}",
        "textquotedblright" => "\u{201d}",
        "textquoteleft" => "\u{2018}",
        "textquoteright" => "\u{2019}",
        "textsection" => "\u{a7}",
        "textunderscore" => "_",
        "textvisiblespace" => "\u{2423}",
        "textacutedbl" => "\u{2dd}",
        "textasciiacute" => "\u{b4}",
        "textasciibreve" => "\u{2d8}",
        "textasciicaron" => "\u{2c7}",
        "textasciidieresis" => "\u{a8}",
        "textasciimacron" => "\u{af}",
        "textgravedbl" => "\u{2f5}",
        "texttildelow" => "\u{2f7}",
        "textbaht" => "\u{e3f}",
        "textcent" => "\u{a2}",
        "textcolonmonetary" => "\u{20a1}",
        "textcurrency" => "\u{a4}",
        "textdollar" => "$",
        "textdong" => "\u{20ab}",
        "texteuro" => "\u{20ac}",
        "textflorin" => "\u{192}",
        "textguarani" => "\u{20b2}",
        "textlira" => "\u{20a4}",
        "textnaira" => "\u{20a6}",
        "textpeso" => "\u{20b1}",
        "textsterling" => "\u{a3}",
        "textwon" => "\u{20a9}",
        "textyen" => "\u{a5}",
        "textcircledP" => "\u{2117}",
        "textcompwordmark" => "\u{200c}",
        "textcopyleft" => "\u{1f12f}",
        "textcopyright" => "\u{a9}",
        "textregistered" => "\u{ae}",
        "textservicemark" => "\u{2120}",
        "texttrademark" => "\u{2122}",
        "textbardbl" => "\u{2016}",
        "textbigcircle" => "\u{25ef}",
        "textblank" => "\u{2422}",
        "textbrokenbar" => "\u{a6}",
        "textdiscount" => "\u{2052}",
        "textestimated" => "\u{212e}",
        "textinterrobang" => "\u{203d}",
        "textinterrobangdown" => "\u{2e18}",
        "textmusicalnote" => "\u{266a}",
        "textnumero" => "\u{2116}",
        "textopenbullet" => "\u{25e6}",
        "textpertenthousand" => "\u{2031}",
        "textperthousand" => "\u{2030}",
        "textrecipe" => "\u{211e}",
        "textreferencemark" => "\u{203b}",
        "textlangle" => "\u{2329}",
        "textrangle" => "\u{232a}",
        "textlbrackdbl" => "\u{27e6}",
        "textrbrackdbl" => "\u{27e7}",
        "textlquill" => "\u{2045}",
        "textrquill" => "\u{2046}",
        "textcelsius" => "\u{2103}",
        "textdegree" => "\u{b0}",
        "textdiv" => "\u{f7}",
        "textdownarrow" => "\u{2193}",
        "textfractionsolidus" => "\u{2044}",
        "textleftarrow" => "\u{2190}",
        "textlnot" => "\u{ac}",
        "textmho" => "\u{2127}",
        "textminus" => "\u{2212}",
        "textmu" => "\u{b5}",
        "textohm" => "\u{2126}",
        "textonehalf" => "\u{bd}",
        "textonequarter" => "\u{bc}",
        "textonesuperior" => "\u{b9}",
        "textpm" => "\u{b1}",
        "textrightarrow" => "\u{2192}",
        "textsurd" => "\u{221a}",
        "textthreequarters" => "\u{be}",
        "textthreesuperior" => "\u{b3}",
        "texttimes" => "\u{d7}",
        "texttwosuperior" => "\u{b2}",
        "textuparrow" => "\u{2191}",
        "textborn" => "\u{2a}",
        "textdied" => "\u{2020}",
        "textdivorced" => "\u{26ae}",
        "textmarried" => "\u{26ad}",
        "textcentoldstyle" => "\u{a2}",
        "textdollaroldstyle" => "$",
        "textzerooldstyle" => "0",
        "textoneoldstyle" => "1",
        "texttwooldstyle" => "2",
        "textthreeoldstyle" => "3",
        "textfouroldstyle" => "4",
        "textfiveoldstyle" => "5",
        "textsixoldstyle" => "6",
        "textsevenoldstyle" => "7",
        "texteightoldstyle" => "8",
        "textnineoldstyle" => "9",
        _ => return None,
    })
}
