//! The TeX packages `lib/mathJaxEngine.ts` loads, a test or several each, read off MathJax's own source.

use super::*;

/// What MathJax defines and this deliberately leaves to `noundefined`'s red, each with why. Everything else in the list has a meaning here.
const LEFT_OUT: &[&str] = &[
    // `textmacros` hands these to the `html` package, which upstream leaves out on purpose: they would put markup of the formula's choosing in the page.
    "href", "style", "class", "data", "cssId",
];

#[test]
fn every_command_of_every_package_upstream_loads_is_known() {
    let mut unknown = Vec::new();
    for line in probe::MATHJAX_COMMANDS.lines().filter(|l| !l.starts_with('#')) {
        let mut parts = line.split('\t');
        let (package, kind, name) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        if LEFT_OUT.contains(&name) {
            continue;
        }
        let source = match kind {
            "E" => format!("\\begin{{{name}}}\\end{{{name}}}"),
            "D" => format!("\\left{name} a\\right."),
            "T" => format!("\\text{{\\{name}{{a}}}}"),
            _ => format!("\\{name}{{a}}{{b}}{{c}}{{d}}"),
        };
        let tree = probe::tex_tree(&source);
        let undefined = format!("Undefined(\"\\\\{}\")", name.replace('\\', "\\\\"));
        let known = match (&tree, kind) {
            (Ok(tree), _) => !tree.contains(&undefined),
            // Its arguments were wrong, which is a meaning. An environment with no name is not.
            (Err(message), "E") => !message.starts_with("Unknown environment"),
            // A delimiter it did not recognize.
            (Err(_), "D") => false,
            (Err(_), _) => true,
        };
        if !known {
            unknown.push(format!("{package} {kind} {name}: {tree:?}"));
        }
    }
    assert!(unknown.is_empty(), "{}", unknown.join("\n"));
}

#[test]
fn a_package_listed_later_wins_as_it_does_in_mathjax() {
    // `physics` has its own `\div`, which is a divergence, and `\divsymbol` is what is left of the old one.
    let tree = parsed(r"\div");
    assert!(tree.contains("∇"), "{tree}");
    assert!(parsed(r"\divsymbol").contains("÷"));
    // `\sin(x)` stretches its parentheses, and `\sin[2](x)` is a power.
    assert!(parsed(r"\sin(\frac{a}{b})").contains("Fenced"));
    assert!(parsed(r"\sin[2](x)").contains("Scripts"));
    assert!(parsed(r"\Re{z}").contains("Fenced"), "`\\Re` is physics' operator, not the letter");
    // `\ket` is physics' rather than braket's, and takes a star.
    assert!(parsed(r"\ket{a}").contains("Fenced"));
    assert!(!parsed(r"\ket*{a}").contains("Fenced"));
    assert!(parsed(r"\Ket{a}").contains("Fenced"), "what only braket has is still there");
}

#[test]
fn an_auto_closing_fence_ends_where_physics_says() {
    // Its own opener seen again is counted; a brace or a `\left` in between is a level of its own.
    assert!(probe::tex_tree(r"\qty(a(b)c)").is_ok());
    assert_eq!(probe::tex_tree(r"\qty(a").unwrap_err(), "Extra open or missing close delimiter");
    assert_eq!(probe::tex_tree(r"\sin(x").unwrap_err(), "Extra open or missing close delimiter");
    assert!(probe::tex_tree(r"\qty({)})").is_ok(), "a closer in a brace is that brace's");
    assert!(probe::tex_tree(r"\qty(\left(a\right))").is_ok());
    assert!(probe::tex_tree(r"\eval(x)|_0^1").is_ok());
    // More arguments than one to a `\pdv` and what is in the parentheses goes.
    let dropped = parsed(r"\pdv{f}{x}{y}(zzz)");
    assert!(!dropped.contains("𝑧") && !dropped.contains('z'), "{dropped}");
}

#[test]
fn physics_matrices_write_themselves_out() {
    assert_eq!(parsed(r"\mqty(\imat{3})").matches("\"1\"").count(), 3);
    assert_eq!(parsed(r"\mqty(\zmat{2}{3})").matches("\"0\"").count(), 6);
    assert_eq!(parsed(r"\mqty(\pmat{1})").matches("\"1\"").count(), 2);
    assert!(parsed(r"\mqty(\xmat*{a}{2}{2})").contains("Scripts"));
    assert_eq!(parsed(r"\mqty(\dmat{1,2,3})").matches("rows:").count(), 4);
    assert_eq!(probe::tex_tree(r"\imat{x}").unwrap_err(), "Invalid number");
    // A bare one has nothing to put its rows in.
    assert!(probe::tex_tree(r"\imat{2}").is_err());
}

#[test]
fn a_definition_holds_for_the_formulas_after_it_and_begingroup_scopes_it() {
    // One input jax, as upstream has one: what it was taught, it knows from then on.
    assert!(probe::tex_tree(r"\newcommand{\persisted}{y}\persisted").is_ok());
    assert!(!parsed(r"\persisted").contains("Undefined"));
    assert!(probe::tex_tree(r"\begingroup\newcommand{\scoped}{y}\scoped\endgroup").is_ok());
    assert!(parsed(r"\scoped").contains("Undefined"));
    assert!(probe::tex_tree(r"\begingroup\global\def\everywhere{y}\endgroup").is_ok());
    assert!(!parsed(r"\everywhere").contains("Undefined"));
    assert_eq!(probe::tex_tree(r"\endgroup").unwrap_err(), "Extra \\endgroup or missing \\begingroup");
    assert_eq!(probe::tex_tree(r"\global x").unwrap_err(), "Invalid use of \\global");
    assert!(probe::tex_tree(r"\newcommand{bare}{y}").is_ok(), "the backslash is optional");
    assert!(!parsed(r"\bare").contains("Undefined"));
    assert_eq!(probe::tex_tree(r"\newcommand{a b}{y}").unwrap_err(), "Illegal control sequence name for \\newcommand");
    assert!(!parsed(r"\let\aliased\frac \aliased{a}{b}").contains("Undefined"));
    assert!(parsed(r"\let\aliased\frac \aliased{a}{b}").contains("Frac"));
    assert!(parsed(r"\newenvironment{mine}[1]{\begin{matrix}#1&}{\end{matrix}}\begin{mine}{a}b\end{mine}").contains("Array"));
    assert!(parsed(r"\def\pair#1,#2.{#2#1}\pair ab,cd.").contains("Sym"));
    assert!(probe::tex_tree(r"\def\pair#1,#2.{x}\pair ab").is_err());
    assert!(parsed(r"\newcommand{\opt}[2][d]{#1#2}\opt{e}").matches("Sym").count() == 2);
}

#[test]
fn colors_can_be_defined() {
    let mut text = text_ctx();
    assert!(probe::tex_tree(r"\definecolor{mine}{RGB}{1,2,3}").is_ok());
    let runs = probe::formula_runs(&mut text, r"\color{mine}x", false, 16.0);
    assert_eq!(runs[0].4, Some(Color::rgb(0x010203)));
}

fn rules_of(source: &str) -> Vec<[f32; 4]> {
    let mut text = text_ctx();
    probe::formula_rules(&mut text, source, true, 16.0)
}

#[test]
fn bbox_pads_fills_and_borders_what_it_holds() {
    let mut text = text_ctx();
    let plain = probe::formula_metrics(&mut text, "x", true, 16.0).unwrap();
    let padded = probe::formula_metrics(&mut text, r"\bbox[5pt]{x}", true, 16.0).unwrap();
    let pad = 0.5 * 16.0 * text.math_scale();
    assert!((padded.0 - plain.0 - 2.0 * pad).abs() < 1.0, "{padded:?} against {plain:?}");
    assert!((padded.1 - plain.1 - pad).abs() < 1.0, "{padded:?} against {plain:?}");
    assert!((padded.2 - plain.2 - pad).abs() < 1.0, "{padded:?} against {plain:?}");
    // A background is one rectangle, a border four.
    assert_eq!(rules_of(r"\bbox[#ff0000]{x}").len(), 1);
    assert_eq!(rules_of(r"\bbox[2pt,border:1px solid red]{x}").len(), 4);
    assert_eq!(rules_of(r"\bbox[border-left:1px solid red]{x}").len(), 1);
    assert_eq!(rules_of(r"\bbox[yellow,3pt,border:2px solid green]{x}").len(), 5);
    assert!(rules_of(r"\bbox[border:1px dashed red]{xxxxxxxxxxxxxx}").len() > 4, "dashes are several");
    // As `BboxMethods` complains.
    assert_eq!(probe::tex_tree(r"\bbox[1pt,2pt]{x}").unwrap_err(), "Padding specified twice in \\bbox");
    assert_eq!(probe::tex_tree(r"\bbox[red,blue]{x}").unwrap_err(), "Background specified twice in \\bbox");
    assert!(probe::tex_tree(r"\bbox[#12]{x}").unwrap_err().contains("doesn't look like a color"));
}

#[test]
fn enclose_draws_each_notation() {
    let mut text = text_ctx();
    let plain = probe::formula_metrics(&mut text, "x+y", true, 16.0).unwrap();
    let paths = |text: &mut TextCtx, source: &str| probe::formula_paths(text, source, true, 16.0).len();
    let rules = |text: &mut TextCtx, source: &str| probe::formula_rules(text, source, true, 16.0).len();
    let wider = |text: &mut TextCtx, source: &str| probe::formula_metrics(text, source, true, 16.0).unwrap().0 > plain.0;
    assert_eq!(rules(&mut text, r"\enclose{box}{x+y}"), 4);
    assert!(wider(&mut text, r"\enclose{box}{x+y}"));
    assert_eq!(paths(&mut text, r"\enclose{roundedbox}{x+y}"), 1);
    assert_eq!(paths(&mut text, r"\enclose{circle}{x+y}"), 1);
    assert!(wider(&mut text, r"\enclose{circle}{x+y}"));
    for side in ["top", "bottom", "left", "right", "horizontalstrike", "verticalstrike"] {
        assert_eq!(rules(&mut text, &format!(r"\enclose{{{side}}}{{x+y}}")), 1, "{side}");
    }
    assert_eq!(rules(&mut text, r"\enclose{actuarial}{x+y}"), 2);
    assert_eq!(rules(&mut text, r"\enclose{madruwb}{x+y}"), 2);
    assert_eq!(paths(&mut text, r"\enclose{updiagonalstrike}{x+y}"), 1);
    assert_eq!(paths(&mut text, r"\enclose{updiagonalstrike,downdiagonalstrike}{x+y}"), 2);
    assert_eq!(paths(&mut text, r"\enclose{longdiv}{x+y}"), 1);
    assert_eq!(paths(&mut text, r"\enclose{radical}{x+y}"), 1);
    assert_eq!(paths(&mut text, r"\enclose{phasorangle}{x+y}"), 1);
    for arrow in ["updiagonalarrow", "northeastarrow", "southeastarrow", "southwestarrow", "northwestarrow", "uparrow", "downarrow", "leftarrow", "rightarrow", "updownarrow", "leftrightarrow"] {
        assert_eq!(paths(&mut text, &format!(r"\enclose{{{arrow}}}{{x+y}}")), 1, "{arrow}");
    }
    assert_eq!(probe::tex_tree(r"\enclose{box}[nonsense=1]{x}").unwrap_err(), "Invalid option: nonsense");
    // Its options reach the drawing: a fill behind, and a colour on the lines.
    let filled = probe::formula_rules(&mut text, r"\enclose{box}[mathbackground=yellow]{x}", true, 16.0);
    assert_eq!(filled.len(), 5);
    let thin = probe::formula_metrics(&mut text, r"\enclose{box}[data-padding=0pt]{x+y}", true, 16.0).unwrap();
    assert!(thin.0 < probe::formula_metrics(&mut text, r"\enclose{box}{x+y}", true, 16.0).unwrap().0);
    // `\cancel` is the same machinery.
    assert_eq!(paths(&mut text, r"\cancel{x}"), 1);
    assert_eq!(paths(&mut text, r"\xcancel{x}"), 2);
    assert_eq!(paths(&mut text, r"\cancel[mathcolor=red]{x}"), 1);
    assert!(paths(&mut text, r"\cancelto{0}{x}") >= 1);
    assert!(parsed(r"\cancelto{0}{x}").contains("Scripts"));
}

#[test]
fn a_commutative_diagram_lines_its_arrows_up() {
    let source = r"\begin{CD} A @>a>> B @>b>> C \\ @VcVV @AAdA @| \\ D @<<e< E @= F \end{CD}";
    let tree = parsed(source);
    assert_eq!(tree.matches("kind:").count(), 7, "{tree}");
    let mut text = text_ctx();
    let (w, h, d) = probe::formula_metrics(&mut text, source, true, 16.0).unwrap();
    assert!(w > 16.0 * 2.75 * 2.0, "two arrows at their least width: {w}");
    assert!(h + d > 16.0 * 1.75 + 16.0, "a row of arrows at its least height: {h} {d}");
    // Objects keep their columns: the two rows' first and last letters are in line.
    let runs = probe::formula_runs(&mut text, source, true, 16.0);
    let at = |letter: &str| {
        let shown = letter.chars().next().unwrap() as u32 - 'A' as u32 + 0x1D434;
        let shown = char::from_u32(shown).unwrap().to_string();
        runs.iter().find(|r| r.0 == shown || r.0 == letter).map(|r| (r.1, r.0.clone())).unwrap_or_else(|| panic!("{letter} in {runs:?}"))
    };
    assert!((at("A").0 - at("D").0).abs() < 4.0, "{:?} {:?}", at("A"), at("D"));
    assert!((at("C").0 - at("F").0).abs() < 4.0, "{:?} {:?}", at("C"), at("F"));
    // The least sizes can be asked for, and an arrow's labels widen it.
    let wide = probe::formula_metrics(&mut text, r"\minCDarrowwidth{8em}\begin{CD}A@>>>B\end{CD}", true, 16.0).unwrap();
    assert!(wide.0 > 16.0 * 8.0, "{wide:?}");
    let labelled = probe::formula_metrics(&mut text, r"\begin{CD}A@>{\text{a long label of words}}>>B\end{CD}", true, 16.0).unwrap();
    assert!(labelled.0 > 16.0 * 6.0, "{labelled:?}");
    // Outside a diagram an `@` is a character.
    assert!(parsed("a@b").contains("@"));
}

#[test]
fn a_proof_tree_stacks_its_rules() {
    let source = r"\begin{prooftree}\AxiomC{$A$}\AxiomC{$B$}\RightLabel{$\wedge$I}\BinaryInfC{$A\wedge B$}\end{prooftree}";
    let tree = parsed(source);
    assert!(tree.contains("Proof"), "{tree}");
    let mut text = text_ctx();
    let rules = probe::formula_rules(&mut text, source, true, 16.0);
    assert_eq!(rules.len(), 1, "one inference, one rule");
    let runs = probe::formula_runs(&mut text, source, true, 16.0);
    let a = "\u{1D434}";
    let premise = runs.iter().find(|r| r.0 == a && r.2 < 0.0).expect("the premise over the line");
    let conclusion = runs.iter().find(|r| r.0 == a && r.2 == 0.0).expect("the conclusion on the baseline");
    assert!(premise.2 < rules[0][1], "{premise:?} over {:?}", rules[0]);
    assert!(rules[0][1] < conclusion.2, "the line is between them");
    // The rule covers the two premises' conclusions, and the label stands beyond its end.
    let b = runs.iter().find(|r| r.0 == "\u{1D435}" && r.2 < 0.0).expect("the other premise");
    assert!(rules[0][0] <= premise.1 && rules[0][0] + rules[0][2] >= b.1);
    let label = runs.iter().find(|r| r.0 == "I").expect("the label");
    assert!(label.1 >= rules[0][0] + rules[0][2], "{label:?} {:?}", rules[0]);

    // A nested tree, `\noLine`, a dashed one, a root on top.
    assert_eq!(probe::formula_rules(&mut text, r"\begin{prooftree}\AXC{a}\noLine\UIC{b}\UIC{c}\end{prooftree}", true, 16.0).len(), 1);
    assert!(probe::formula_rules(&mut text, r"\begin{prooftree}\AXC{a}\dashedLine\UIC{b}\end{prooftree}", true, 16.0).len() > 1);
    assert!(parsed(r"\begin{prooftree}\rootAtTop\AXC{a}\UIC{b}\end{prooftree}").contains("root_at_top: true"));
    assert!(parsed(r"\begin{prooftree}\Axiom$a\fCenter b$\UnaryInf$c\fCenter d$\end{prooftree}").contains("centered: Some"));
    // What MathJax refuses.
    assert_eq!(probe::tex_tree(r"\AxiomC{a}").unwrap_err(), "Proof commands only allowed in prooftree environment.");
    assert_eq!(probe::tex_tree(r"\begin{prooftree}\BinaryInfC{a}\end{prooftree}").unwrap_err(), "Proof tree badly specified.");
    assert_eq!(probe::tex_tree(r"\begin{prooftree}\AXC{a}\AXC{b}\end{prooftree}").unwrap_err(), "Proof tree badly specified.");
}

#[test]
fn a_table_of_math_takes_its_colors() {
    let plain = rules_of(r"\begin{array}{cc}a&b\\c&d\end{array}").len();
    assert_eq!(rules_of(r"\begin{array}{cc}\cellcolor{red}a&b\\c&d\end{array}").len(), plain + 1);
    assert_eq!(rules_of(r"\begin{array}{cc}\rowcolor{red}a&b\\c&d\end{array}").len(), plain + 2);
    assert_eq!(rules_of(r"\begin{array}{cc}\columncolor{red}a&b\\c&d\end{array}").len(), plain + 2);
    assert_eq!(probe::tex_tree(r"\cellcolor{red}x").unwrap_err(), "Unsupported use of \\cellcolor");
    assert_eq!(probe::tex_tree(r"\begin{array}{c}a\\\columncolor{red}b\end{array}").unwrap_err(), "\\columncolor must be in the top row or preamble");
    // A column spec's rules, dashes and what it puts around a cell.
    assert_eq!(rules_of(r"\begin{array}{c|c}a&b\end{array}").len(), plain + 1);
    assert!(rules_of(r"\begin{array}{c:c}a&b\end{array}").len() > plain + 1);
    assert!(rules_of(r"\begin{array}{cc}\hdashline a&b\end{array}").len() > plain + 1);
    assert!(parsed(r"\begin{array}{>{\bf}c}a\end{array}").contains("Bold"));
    assert!(parsed(r"\begin{array}{c<{x}}a\end{array}").contains("𝑥") || parsed(r"\begin{array}{c<{x}}a\end{array}").contains("\"x\""));
    assert!(parsed(r"\newcolumntype{Q}{c|c}\begin{array}{Q}a&b\end{array}").contains("rules: [0, 1, 0]"));
    assert_eq!(probe::tex_tree(r"\begin{array}{c}a\hline\end{array}").unwrap_err(), "Misplaced \\hline");
}

#[test]
fn an_equation_structure_has_to_be_the_whole_of_the_formula() {
    assert!(probe::tex_tree(r"\begin{align}a&=b\end{align}").is_ok());
    assert_eq!(probe::tex_tree(r"x\begin{align}a&=b\end{align}").unwrap_err(), "Erroneous nesting of equation structures");
    assert_eq!(probe::tex_tree(r"{\begin{align}a&=b\end{align}}").unwrap_err(), "Erroneous nesting of equation structures");
    assert!(probe::tex_tree(r"x\begin{aligned}a&=b\end{aligned}").is_ok());
    assert!(probe::tex_tree(r"\begin{empheq}[left=\empheqlbrace]{align}a&=b\end{empheq}").is_ok());
}

#[test]
fn empheq_stretches_its_delimiter_over_the_alignment() {
    let source = r"\begin{empheq}[left={x=\empheqlbrace}]{align}a&=b\\c&=d\\e&=f\end{empheq}";
    let mut text = text_ctx();
    let (_, h, d) = probe::formula_metrics(&mut text, source, true, 16.0).unwrap();
    assert!(h + d > 16.0 * 2.5, "{h} {d}");
    let paths = probe::formula_paths(&mut text, source, true, 16.0);
    assert_eq!(paths.len(), 1, "the brace is drawn to the height: {paths:?}");
    assert!(paths[0].1 as f32 > 16.0 * 2.0, "{paths:?}");
    assert!(probe::tex_tree(r"\begin{empheq}{aligned}a\end{empheq}").unwrap_err().contains("Invalid environment"));
    assert!(probe::tex_tree(r"\begin{empheq}[box=\fbox]{align}a\end{empheq}").unwrap_err().contains("Invalid option"));
}

#[test]
fn numcases_and_starred_cases_read_their_second_column_as_text() {
    let tree = parsed(r"\begin{numcases}{f(x)=} 1 & if $x>0$ \\ 0 & otherwise \end{numcases}");
    assert!(tree.contains("Text { text: \"otherwise"), "{tree}");
    assert!(tree.contains("left: '{'"), "{tree}");
    let starred = parsed(r"\begin{cases*} 1 & when $x$ is odd \\ 0 & otherwise \end{cases*}");
    assert!(starred.contains("when"), "{starred}");
    assert_eq!(probe::tex_tree(r"\begin{cases*} 1 & a & b \end{cases*}").unwrap_err(), "Extra alignment tab in text for numcase environment");
    assert!(parsed(r"\begin{rcases*} 1 & a \end{rcases*}").contains("right: '}'"));
}

#[test]
fn mathtools_commands_do_what_their_methods_do() {
    let mut text = text_ctx();
    // Colons sit where it puts them, as relations.
    assert!(parsed(r"a\coloneqq b").contains("Classed(Rel"));
    assert!(parsed(r"a\dblcolon b").contains("Classed(Rel"));
    // The `*` of a paired delimiter stretches it, and its bracket says how big.
    assert!(probe::tex_tree(r"\DeclarePairedDelimiter\cabs{\lvert}{\rvert}").is_ok());
    assert!(parsed(r"\cabs*{\frac{a}{b}}").contains("Fenced"));
    assert!(parsed(r"\cabs[\big]{a}").contains("Big"));
    assert!(!parsed(r"\cabs{a}").contains("Fenced"));
    assert!(probe::tex_tree(r"\DeclarePairedDelimiterX\inner[2]{\langle}{\rangle}{#1,#2}").is_ok());
    assert!(parsed(r"\inner*{a}{b}").contains("Fenced"));
    // Prescripts stand to the left of the base.
    let runs = probe::formula_runs(&mut text, r"\prescript{a}{b}{X}", true, 16.0);
    assert!(runs.iter().position(|r| r.0 == "\u{1D44E}") < runs.iter().position(|r| r.0 == "\u{1D44B}"), "{runs:?}");
    let base = runs.iter().find(|r| r.0 == "\u{1D44B}").unwrap();
    assert!(runs.iter().find(|r| r.0 == "\u{1D44E}").unwrap().1 < base.1);
    assert_eq!(probe::tex_tree(r"\shoveleft{a}").unwrap_err(), "\\shoveleft can only appear within the multline or multlined environments");
    // A matrix aligned as asked.
    assert!(parsed(r"\begin{pmatrix*}[r]1&2\end{pmatrix*}").contains("columns: ['r']"));
    assert!(parsed(r"\begin{bsmallmatrix}1&2\end{bsmallmatrix}").contains("left: '['"));
    // Brackets over and under, `\splitfrac`, a `\vdotswithin` as wide as what it stands under.
    assert_eq!(probe::formula_rules(&mut text, r"\overbracket{x}", true, 16.0).len(), 3);
    assert_eq!(probe::formula_rules(&mut text, r"\underbracket[2pt][1em]{x}", true, 16.0).len(), 3);
    assert!(parsed(r"\splitfrac{a}{b}").contains("align: (-1, 1)"));
    let (w, ..) = probe::formula_metrics(&mut text, r"\vdotswithin{=}", true, 16.0).unwrap();
    let (eq, ..) = probe::formula_metrics(&mut text, r"{}={}", true, 16.0).unwrap();
    assert!((w - eq).abs() < 2.0, "{w} against {eq}");
    assert!(parsed(r"\begin{align}\Aboxed{a&=b}\\c&=d\end{align}").contains("Boxed"));
    assert!(probe::tex_tree(r"\Aboxed{a&=b}").is_err());
    assert!(probe::tex_tree(r"\begin{align}\MoveEqLeft a&=b\\\ArrowBetweenLines c&=d\end{align}").is_ok());
    assert!(parsed(r"\adjustlimits\lim_{n}\sup_{m}").contains("Phantom"));
    assert!(parsed(r"\newtagform{brackets}{[}{]}\usetagform{brackets}\tag{1}x").contains("Tag"));
    assert!(parsed(r"\tag{1}x").contains("\"[\""), "the form stays chosen");
    assert!(probe::tex_tree(r"\usetagform{default}").is_ok());
    assert_eq!(probe::tex_tree(r"\usetagform{nothing}").unwrap_err(), "Undefined tag form 'nothing'");
}

#[test]
fn units_unicode_and_the_rest_of_the_small_packages() {
    let mut text = text_ctx();
    assert!(parsed(r"\units[5]{kg}").contains("\"kg\""));
    assert!(parsed(r"\nicefrac{1}{2}").contains("bevelled: true"));
    assert!(parsed(r"\unitfrac[3]{m}{s}").contains("bevelled: true"));
    assert!(!probe::formula_paths(&mut text, r"\nicefrac{1}{2}", true, 16.0).is_empty() || !probe::formula_runs(&mut text, r"\nicefrac{1}{2}", true, 16.0).is_empty());
    assert!(parsed(r"\U{3B1}").contains("α"));
    assert!(probe::tex_tree(r"\U{xyz}").is_err());
    assert!(parsed(r#"\char"3B1 "#).contains("α"));
    assert!(parsed(r"\char'101 ").contains("\"A\""));
    assert!(parsed(r"\char`A").contains("\"A\""));
    assert!(parsed(r"\char65 ").contains("\"A\""));
    assert!(parsed(r"\upzeta \Upomega").contains("ζ"));
    assert!(parsed(r"\centernot{=}").contains("CenterOver"));
    let (w, ..) = probe::formula_metrics(&mut text, r"\centernot{=}", true, 16.0).unwrap();
    let (eq, ..) = probe::formula_metrics(&mut text, r"=", true, 16.0).unwrap();
    assert!((w - eq).abs() < 1.0, "as wide as what it stands on: {w} {eq}");
    assert!(probe::tex_tree(r"\Newextarrow{\myto}{5,10}{0x21A0}").is_ok());
    assert!(parsed(r"\myto{a}").contains("Arrow"));
    assert!(parsed(r"\xlongequal{a}").contains("Equal"));
    assert!(parsed(r"\xtofrom{a}").contains("ToFrom"));
    assert_eq!(probe::tex_tree(r"\Newextarrow{x}{5,10}{1}").unwrap_err(), "First argument to \\Newextarrow must be a control sequence name");
    assert!(parsed(r"\textdegree").contains('°'));
    assert!(parsed(r"\text{\textdegree}").contains('°'));
    assert!(parsed(r"\sideset{_a^b}{_c^d}\sum").contains("SideSet"));
    assert!(parsed(r"\mathchoice{a}{b}{c}{d}").contains("Choice"));
    assert_eq!(run_of(&mut text, r"\mathchoice{a}{b}{c}{d}", true, "\u{1D44E}").0 >= 0.0, true);
    assert!(probe::formula_runs(&mut text, r"\mathchoice{a}{b}{c}{d}", false, 16.0).iter().all(|r| r.0 == "\u{1D44F}"));
    assert!(parsed(r"\mmlToken{mo}[texClass=BIN]{mod}").contains("class: Bin"));
    assert_eq!(probe::tex_tree(r"\mmlToken{mrow}{x}").unwrap_err(), "mrow is not a token element");
    assert!(parsed(r"\matrix{a&b\cr c&d}").contains("Array"));
    assert!(parsed(r"\eqalign{a&=b\cr c&=d}").contains("paired: true"));
    assert!(parsed(r"\vcenter{a}").contains("VBox"));
    assert!(parsed(r"\sqrt[\uproot{3}\leftroot{2}n]{x}").contains("up: 0.2"));
    assert_eq!(probe::tex_tree(r"\uproot{2}").unwrap_err(), "\\uproot can appear only within a root");
    assert!(parsed(r"{a\overwithdelims() b}").contains("left: '('"));
    assert!(parsed(r"\ddddot{x}").contains("Ddddot"));
    assert!(parsed(r"\overparen{ab}").contains("OverParen"));
    assert!(parsed(r"\tag{7}\label{mine}x").contains("Tag"));
    assert!(parsed(r"\ref{mine}").contains("\"7\""), "a label keeps what it was tagged");
    assert!(parsed(r"\eqref{mine}").contains("\"(7)\""));
    assert!(parsed(r"\ref{never}").contains("###"));
}

#[test]
fn text_mode_has_its_own_commands() {
    let mut text = text_ctx();
    assert_eq!(probe::tex_tree(r"\text{\alpha}").unwrap_err(), "\\alpha is only supported in math mode");
    assert!(parsed(r"\text{\nonsense}").contains("Undefined"));
    assert_eq!(probe::tex_tree(r"\text{a_b}").unwrap_err(), "'_' allowed only in math mode");
    // Fonts hold to the end of their group, and a brace is one.
    let tree = parsed(r"\text{{\bf a} b}");
    assert!(tree.contains("Text { text: \"a\", font: Bold }"), "{tree}");
    assert!(tree.contains("Text { text: \" b\", font: Roman }"), "{tree}");
    assert!(parsed(r"\text{\emph{a}}").contains("Italic"));
    assert!(parsed(r"\text{\it \emph{a}}").contains("Text { text: \"a\", font: Roman }"), "emphasis inside italics stands upright");
    assert!(parsed(r"\text{\Large a}").contains("SetSize(1.2)"));
    assert!(parsed(r"\text{\'e}").contains("e\\u{301}"));
    assert!(parsed(r"\text{``a''}").contains("“a”"));
    assert!(parsed(r"\text{\underline{a}}").contains("Underline"));
    assert!(parsed(r"\text{\textcolor{red}{a}}").contains("Colored"));
    assert!(parsed(r"\text{\color{red}a}").contains("SetColor"));
    assert!(parsed(r"\text{\hspace{1em}a}").contains("Space"));
    assert!(parsed(r"\text{\rlap{a}}").contains("Lap"));
    assert!(parsed(r"\text{\(x\) and $y$}").matches("Group").count() >= 2);
    assert!(parsed(r"\text{\ldots}").contains('…'));
    assert!(probe::formula_metrics(&mut text, r"\text{a\quad b}", true, 16.0).unwrap().0 > 16.0);
}

#[test]
fn mhchem_writes_what_its_parser_writes() {
    let ce = |source: &str| parsed(&format!("\\ce{{{source}}}"));
    let mut text = text_ctx();
    // Elements upright, a number after one down, a charge up.
    let water = ce("H2O");
    assert!(water.contains("\"H\""), "{water}");
    assert!(water.contains("\"O\""));
    assert!(water.contains("sub: Some"));
    assert!(ce("Fe^{3+}").contains("sup: Some"));
    assert!(ce("SO4^2-").contains("sup: Some"));
    assert!(ce("Na+").contains("sup: Some"));
    assert!(ce("Ca2+").contains("sup: Some"));
    // Stoichiometric numbers stand before a thin space, and states and arrows are what they are.
    assert!(ce("2H2 + O2 -> 2H2O").contains("⟶"), "{}", ce("2H2 + O2 -> 2H2O"));
    assert!(ce("A <=> B").contains("⇌"));
    assert!(ce("A <--> B").contains("⇄"));
    assert!(ce("A ->[{heat}][{below}] B").contains("Arrow"));
    assert!(ce("NaCl(aq)").contains("\"aq\""));
    assert!(ce("CO2 ^").contains("↑"));
    assert!(ce("AgCl v").contains("↓"));
    // Isotopes, bonds, fractions, oxidation states, addition compounds.
    assert!(ce("^{227}_{90}Th+").contains("Lap"));
    assert!(ce("C=C").contains("\"=\""));
    assert!(ce("C#C").contains("≡"));
    assert!(ce("1/2 H2O").contains("Choice"));
    assert!(ce("Fe^{II}").contains("\"II\"") || ce("Fe^{II}").contains("II"));
    assert!(ce("CuSO4 . 5H2O").contains("⋅"));
    assert!(ce("\\frac{a}{b}").contains("Frac"));
    assert!(ce("$x$ A").contains("\"x\""));
    assert!(ce("H2O \\text{and} A").contains("and"));
    assert!(ce("pH").contains("\"pH\""));
    assert!(ce("A + B").contains("Bin"));
    // `\pu`: units, numbers with their thousands apart, powers, fractions.
    let pu = |source: &str| parsed(&format!("\\pu{{{source}}}"));
    assert!(pu("123 kJ/mol").contains("\"kJ\""));
    assert!(pu("1.2e5 m").contains("⋅"));
    assert!(pu("1.2*10^3 kJ").contains("⋅"));
    assert!(pu("1.2x10^3 kJ").contains("×"));
    assert!(pu("12345.6 m").contains("Space"));
    assert!(pu("10^-3 m").contains("sup: Some"));
    assert!(pu("kJ//mol").contains("Choice"));
    // A half-written one is what it is, with no panic on the way.
    for source in ["H2O ->", "A ->[", "^{", "\\frac{a", "(", ")", "1/", "$x", "A -", "\\ce{\\ce{", "<=>", "+", "...", "&", "\\\\", "^", "_", "{}^", "α"] {
        let _ = probe::formula_metrics(&mut text, &format!("\\ce{{{source}}}"), true, 16.0);
        let _ = probe::formula_metrics(&mut text, &format!("\\pu{{{source}}}"), true, 16.0);
    }
}

#[test]
fn a_tag_goes_to_the_side_of_the_column_and_a_multline_to_both() {
    let mut text = text_ctx();
    // Centred as `displayAlign` has it, with the tag at the end of however wide the column is.
    let (at, tag, full) = probe::formula_in_column(&mut text, r"x+y\tag{1}", 16.0, 600.0).unwrap();
    assert_eq!(full, 600.0);
    let (w, ..) = probe::formula_metrics(&mut text, r"x+y\tag{1}", true, 16.0).unwrap();
    assert!((at - (600.0 - w) * 0.5).abs() < 1.0, "{at} {w}");
    assert!(tag > 500.0, "{tag}");
    // With no room for both it pushes the box wider, and the formula starts at the left.
    let (at, tag, full) = probe::formula_in_column(&mut text, r"x+y\tag{1}", 16.0, 10.0).unwrap();
    assert!(at.abs() < 0.01, "{at}");
    assert!(full > 10.0 && tag > w, "{full} {tag}");
    // An inline formula has no column to speak of: a tag on it is not drawn.
    assert_eq!(probe::formula_runs(&mut text, r"x\tag{1}", false, 16.0).len(), 1);
    // Without one, it stands where the text does.
    assert_eq!(probe::formula_in_column(&mut text, "x", 16.0, 600.0).unwrap(), (0.0, 0.0, probe::formula_metrics(&mut text, "x", true, 16.0).unwrap().0));
    // On the rows of an alignment.
    let (_, tag, full) = probe::formula_in_column(&mut text, r"\begin{align}a&=b\tag{1}\\c&=d\tag{2}\end{align}", 16.0, 600.0).unwrap();
    assert_eq!(full, 600.0);
    assert!(tag > 500.0);
    assert_eq!(probe::tex_tree(r"\tag{1}\tag{2}x").unwrap_err(), "Multiple \\tag");
    assert_eq!(probe::formula_spread(&mut text, r"\begin{multline}a\\b\\c\end{multline}", 16.0).iter().map(|r| r.1).collect::<Vec<_>>(), [0.0, 0.5, 1.0]);
    assert_eq!(probe::formula_spread(&mut text, r"\begin{multline}a\\\shoveright{b}\\c\end{multline}", 16.0).iter().map(|r| r.1).collect::<Vec<_>>(), [0.0, 1.0, 1.0]);
    // What is not the whole of the formula keeps to its own width.
    assert_eq!(probe::formula_spread(&mut text, r"x\begin{multlined}a\\b\end{multlined}", 16.0).len(), 0);
}

#[test]
fn nothing_new_goes_deep_enough_to_end_the_stack() {
    let run = || {
        let mut text = TextCtx::new(1.0);
        for piece in [r"\sin(", r"\qty(", r"\qty{", r"\vb{", r"\cancel{", r"\enclose{box}{", r"\bbox[1pt]{", r"\begin{empheq}{align}", r"\begin{prooftree}\AXC{", r"\begin{CD}", r"\mathchoice{", r"\sideset{}{}{", r"\text{\underline{", r"\text{{", r"\ce{", r"\ce{\frac{", r"\mqty(", r"\begin{mine}", r"\centernot{", r"\prescript{}{}{", r"\vcenter{", r"\makebox{"] {
            for depth in [38, 39, 40, 41, 300] {
                let source = piece.repeat(depth);
                let _ = probe::formula_metrics(&mut text, &source, true, 13.0);
                let closed = format!("{source}x{}", "}".repeat(depth));
                let _ = probe::formula_metrics(&mut text, &closed, true, 13.0);
                let closed = format!("{source}x{}", ")".repeat(depth));
                let _ = probe::formula_metrics(&mut text, &closed, true, 13.0);
            }
        }
    };
    std::thread::Builder::new().stack_size(1024 * 1024).spawn(run).unwrap().join().unwrap();
}

const PACKAGES_PAGE: &str = r#"**bbox.** $\bbox[5px,border:2px solid red]{x^2+y^2}$ and $\bbox[#fde68a,8px]{\frac{a}{b}}$ and $\bbox[border:1px dashed blue]{\int_0^1 f}$.

**enclose.** $\enclose{circle}{x}$, $\enclose{box}{x+y}$, $\enclose{roundedbox}[mathcolor=green]{a}$, $\enclose{updiagonalstrike,downdiagonalstrike}{ab}$, $\enclose{longdiv}{1234}$, $\enclose{actuarial}{n}$, $\enclose{radical}{x}$, $\enclose{updiagonalarrow}{xy}$, $\enclose{leftrightarrow}{xy}$, $\enclose{phasorangle}{30}$, $\enclose{madruwb}{m}$.

**cancel.** $\cancel{x}\ \bcancel{y}\ \xcancel{z}\ \cancelto{0}{x+y}$

$$\begin{CD} A @>a>> B @>b>> C \\ @VcVV @AdAA @| \\ D @<<e< E @= F \end{CD}$$

$$\begin{prooftree}\AxiomC{$A \to B$}\AxiomC{$A$}\RightLabel{MP}\BinaryInfC{$B$}\AxiomC{$C$}\LeftLabel{$\wedge$I}\BinaryInfC{$B \wedge C$}\end{prooftree}$$

$$\begin{prooftree}\AXC{$\Gamma$}\noLine\UIC{$\vdots$}\dashedLine\UIC{$\Delta$}\UIC{$\Theta$}\end{prooftree}\qquad \begin{prooftree}\rootAtTop\AXC{a}\AXC{b}\BIC{c}\end{prooftree}$$

$$\begin{empheq}[left={f(x)=\empheqlbrace}]{align} a &= b+c \\ d &= e \\ g &= h+i+j \end{empheq}$$

$$\begin{numcases}{g(x)=} 1 & when $x>0$ \\ 0 & otherwise \end{numcases}$$

$$\begin{array}{|>{\bf}c:c|} \hline \rowcolor{#bfdbfe} a & b \\ \hdashline \cellcolor{#fecaca} c & d \\ \hline \end{array} \quad \begin{pmatrix*}[r] 1 & -2 \\ -30 & 4 \end{pmatrix*} \quad \begin{bsmallmatrix} 1 & 2 \\ 3 & 4 \end{bsmallmatrix}$$

$$\begin{multline} a+b+c+d+e+f \\ +g+h+i \\ \shoveleft{+j} \\ +k+l+m+n+o+p+q+r+s+t \end{multline}$$

**physics.** $\qty(\frac{a}{b})$, $\abs{x}$, $\norm{\vb{v}}$, $\comm{A}{B}$, $\acomm{A}{B}$, $\sin(\frac{\pi}{2})$, $\sin[2](x)$, $\eval{x^2}_0^1$, $\order{n^2}$, $\grad{f}$, $\div(\vb{F})$, $\curl(\vb{F})$, $\laplacian{\phi}$, $\va{v}$, $\vu{n}$, $\dd{x}$, $\dv{f}{x}$, $\pdv{f}{x}{y}$, $\fdv{F}{g}$, $\bra{a}\ket{b}$, $\ket{\psi}$, $\ip{a}{b}$, $\dyad{a}{b}$, $\ev{A}{\psi}$, $\mel{a}{H}{b}$, $\mqty(1 & 0 \\ 0 & 1)$, $\mqty[\imat{2}]$, $\mdet{a & b \\ c & d}$, $\smqty(a & b \\ c & d)$, $\mqty(\pmat{1})$. $\qq{for all} x \qif y$

**mathtools.** $a \coloneqq b \dblcolon c \Coloneqq d \eqqcolon e$, $\prescript{14}{6}{\mathrm{C}}$, $\overbracket{abc}$, $\underbracket[1pt][4pt]{abc}$, $\overparen{abc}$, $\underparen{abc}$, $\xleftrightarrow[b]{a}$, $\xlongequal{a}$, $\xtofrom[b]{a}$, $\xmapsto{a}$, $\xhookrightarrow{a}$, $\xtwoheadrightarrow{a}$, $\xrightharpoonup{a}$, $\nuparrow$, $\bigtimes_{i}$, $\splitfrac{a+b}{c+d}$, $\mathclap{abc}$, $\cramped{x^2}$, $\vdotswithin{=}$, $\centernot{\implies}$

$$\sideset{_a^b}{_c^d}\sum_{i=0}^n \quad \adjustlimits\lim_{n\to\infty}\max_{i} \quad \sqrt[\uproot{4}\leftroot{-2} 3]{x} \quad \mathchoice{D}{T}{S}{SS} \quad \nicefrac{1}{2}\ \unitfrac[3]{m}{s}\ \units[5]{kg} \quad \underset{b}{\overset{a}{\longrightarrow}} \quad \text{\textbf{bold} \emph{it} \'e \textdegree C \underline{u}}$$

**mhchem.** $\ce{2H2 + O2 -> 2H2O}$, $\ce{SO4^2- + Ba^2+ -> BaSO4 v}$, $\ce{CO2 + C <=> 2CO}$, $\ce{^{227}_{90}Th+}$, $\ce{CuSO4 . 5H2O}$, $\ce{A ->[{heat}][{cat}] B}$, $\ce{C=C-C#N}$, $\ce{Fe^{II}Fe^{III}2O4}$, $\ce{1/2 H2O}$, $\ce{NaCl(aq)}$, $\ce{CO2 ^}$, $\ce{A <--> B}$, $\pu{123 kJ/mol}$, $\pu{1.2e5 m/s}$, $\pu{12345.67 kg}$, $\pu{1.2x10^3 Pa}$, $\pu{kJ//mol}$

**base.** $\TeX$, $\LaTeX$, $\skew{5}{\hat}{A}$, $\matrix{a&b\cr c&d}$, $\pmatrix{a&b\cr c&d}$, $\cases{a&b\cr c&d}$, $\vcenter{x}$, $\moveleft 1em x$, $\raise.2em x$, $\ddddot{x}$, ${a \overwithdelims() b}$, $\varinjlim_n$, $\varliminf_n$, $\digamma$, $\varGamma$, $\lessapprox$, $\upharpoonleft$, $\TeX\tag{1}$"#;

#[test]
#[ignore]
fn visual_math_packages() {
    let dir = std::env::var("MD_SHOT").expect("where to put the pictures");
    for (theme_name, theme) in [("dark", Theme::dark()), ("light", Theme::light())] {
        let mut rig = Rig::new();
        rig.size = (900.0, 1750.0);
        rig.scale = 2.0;
        rig.theme = theme;
        rig.text = TextCtx::new(2.0);
        let view = move |_: &mut App| col().w_full().h_full().bg(theme.layer).p(20.0).child(markdown_content("math-packages", PACKAGES_PAGE, MarkdownOptions::default()));
        rig.settle(&view);
        rig.shot(&format!("{dir}/math-packages-{theme_name}.png"));
        assert_eq!(probe::math_notes("math-packages"), Vec::<String>::new());
    }
}
