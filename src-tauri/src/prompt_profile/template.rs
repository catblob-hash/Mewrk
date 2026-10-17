//! The tool-description templates: the built-in profile written out as a file
//! a user copies and edits, with a comment above every key saying what the
//! text is and what each placeholder it may use stands for — one template with
//! English comments, one with Chinese ones.
//!
//! They are golden exports, `docs/context-injections/tool-description-template.
//! {en-US,zh-CN}.jsonc`, written with the others by
//! `regenerate_prompt_profile_baselines`, so a template always carries the
//! texts this build ships. A template is JSONC, which a tool-description file
//! may be (`config_file::parse_jsonc`). The comments come from
//! `template_notes.json`, one entry per key; the section headings are the
//! registry's own `// ---- … ----` lines.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

use super::PromptKey;
use crate::tool_surface::ToolVariant;

const NOTES: &str = include_str!("template_notes.json");
const REGISTRY_SOURCE: &str = include_str!("../prompt_profile.rs");

#[derive(Deserialize)]
struct Notes {
    /// Chinese heading for each English registry section heading.
    sections: BTreeMap<String, String>,
    /// When each non-standard tool variant is offered, by variant id.
    variants: BTreeMap<String, Bilingual>,
    keys: HashMap<String, KeyNote>,
}

#[derive(Deserialize)]
struct Bilingual {
    en: String,
    zh: String,
}

#[derive(Deserialize)]
struct KeyNote {
    en: String,
    zh: String,
    /// What each declared placeholder stands for, by name.
    placeholders: BTreeMap<String, Bilingual>,
}

/// The language a template's comments are written in. The texts themselves
/// are the built-in English ones in both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Language {
    English,
    Chinese,
}

impl Language {
    pub(super) const ALL: [Language; 2] = [Language::English, Language::Chinese];

    /// The golden file the template is exported as.
    pub(super) fn file_name(self) -> &'static str {
        match self {
            Language::English => "tool-description-template.en-US.jsonc",
            Language::Chinese => "tool-description-template.zh-CN.jsonc",
        }
    }

    fn pick<'a>(self, en: &'a str, zh: &'a str) -> &'a str {
        match self {
            Language::English => en,
            Language::Chinese => zh,
        }
    }
}

impl Bilingual {
    fn get(&self, language: Language) -> &str {
        language.pick(&self.en, &self.zh)
    }
}

fn notes() -> Notes {
    serde_json::from_str(NOTES).expect("template_notes.json is valid JSON")
}

/// The registry's section headings, by the id of the first key under each.
fn section_starts() -> HashMap<&'static str, &'static str> {
    let mut starts = HashMap::new();
    let mut pending = None;
    for line in REGISTRY_SOURCE.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix("// ---- ") {
            pending = Some(heading.trim_end_matches('-').trim_end());
            continue;
        }
        let Some(heading) = pending else {
            continue;
        };
        if let Some(id) = line
            .split_once(" => (\"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(id, _)| id)
        {
            starts.insert(id, heading);
            pending = None;
        }
    }
    starts
}

/// The non-standard variants the catalog's tools have, in catalog order, each
/// with the tools that have it.
fn variants_in_catalog() -> Vec<(ToolVariant, Vec<String>)> {
    let mut variants: Vec<(ToolVariant, Vec<String>)> = Vec::new();
    for tool in crate::catalog::tool_catalog() {
        for variant in crate::tool_surface::variants_of(&tool.name) {
            if *variant == ToolVariant::Standard {
                continue;
            }
            match variants.iter_mut().find(|(known, _)| known == variant) {
                Some((_, tools)) => tools.push(tool.name.clone()),
                None => variants.push((*variant, vec![tool.name.clone()])),
            }
        }
    }
    variants
}

const HEADER_EN: &[&str] = &[
    "Mewrk tool-description file template (English comments)",
    "It holds the built-in \"Mewrk concise\": every fixed text Mewrk puts into a model request,",
    "and the description of every built-in tool and parameter.",
    "",
    "How to use it",
    "  1. Copy it into ~/.mewrk/tool-descriptions/ (only that global folder is read, never a",
    "     workspace's). Any file name will do, ending in .json or .jsonc.",
    "  2. Edit the texts, then pick the file under Conversation settings → Tool descriptions.",
    "",
    "Rules",
    "  - // and /* */ comments and trailing commas are allowed.",
    "  - Keep only the keys you change. A key you delete uses the built-in text and follows",
    "    Mewrk's updates; a key you keep says what you wrote until you change it.",
    "  - \"\" (an empty string) says nothing at that point; an empty tool or parameter",
    "    description is left out of the request.",
    "  - {name} is a placeholder the host fills in. The comment above a key lists the ones it",
    "    takes; a key whose comment lists none takes none, and an unknown one stays as written.",
    "  - In tool and parameter descriptions, {?tool}…{/} is kept only when this request offers",
    "    that tool, {?a|b}…{/} when it offers either, and {!tool}…{/} when it offers none of them.",
    "    They nest, and @shell stands for any shell tool. The child agent addendum takes them too,",
    "    against the tools that child holds.",
    "  - Generated by Mewrk from the texts this version ships.",
];

const HEADER_ZH: &[&str] = &[
    "Mewrk 工具描述文件模板（中文注释版）",
    "内容就是内置的「Mewrk concise」：Mewrk 放进模型请求的每一段固定文字，",
    "以及每个内置工具和参数的描述。",
    "",
    "怎么用",
    "  1. 复制到 ~/.mewrk/tool-descriptions/（只读这个全局目录，工作区里的不读），",
    "     文件名随意，扩展名用 .json 或 .jsonc。",
    "  2. 改文字，然后在「对话设置 → 工具描述」里选中这份文件。",
    "",
    "规则",
    "  - 可以写 // 和 /* */ 注释，也可以有尾逗号。",
    "  - 只留要改的键就行。删掉的键用内置文字，并随 Mewrk 升级更新；留下的键一直是你写的文字。",
    "  - 值为 \"\"（空字符串）表示这里什么都不说；空的工具或参数描述不会发给模型。",
    "  - {名字} 是占位符，运行时换成实际的值。每个键可用的占位符列在它上方的注释里，",
    "    没列就是不接受占位符；写了不认识的占位符会原样保留。",
    "  - 工具和参数描述里可以写 {?工具名}…{/}：本次请求提供该工具时才保留；{?a|b}…{/}：",
    "    提供其中任一个即保留；{!工具名}…{/}：一个都不提供时才保留。可以嵌套，@shell 表示任意 shell 工具。",
    "    子代理补充说明也接受这些标记，依据该子代理自己持有的工具判断。",
    "  - 本文件由 Mewrk 按这一版的内置文字生成。",
];

fn comment(out: &mut String, indent: &str, text: &str) {
    out.push_str(indent);
    out.push_str("//");
    if !text.is_empty() {
        out.push(' ');
        out.push_str(text);
    }
    out.push('\n');
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

/// The template in `language`, as the text of a JSONC file.
pub(super) fn render(language: Language) -> String {
    let notes = notes();
    let sections = section_starts();
    let l = language;
    let mut out = String::new();
    for line in l.pick_lines(HEADER_EN, HEADER_ZH) {
        comment(&mut out, "", line);
    }
    out.push_str("{\n");

    comment(&mut out, "  ", l.pick(
        "Display name in the tool-description list; without it the file name is used.",
        "显示名称，出现在工具描述列表里；不写就用文件名。",
    ));
    out.push_str(&format!(
        "  \"name\": {},\n\n",
        json_string(l.pick("My tool descriptions", "我的工具描述"))
    ));

    for line in [
        l.pick("Per-tool descriptions. Each entry has:", "按工具改描述。每一项："),
        l.pick(
            "  toolName     a built-in tool's name (\"grep\"), or an MCP tool's \"mcp__<server>__<tool>\".",
            "  toolName     工具名：内置工具名（如 \"grep\"），或 MCP 工具的 \"mcp__<服务器>__<工具>\"。",
        ),
        l.pick(
            "  variant      optional; leave it out for the standard form. Only these tools have others:",
            "  variant      变体，可省略（即标准形态）。只有下列工具有其他变体：",
        ),
    ] {
        comment(&mut out, "  ", line);
    }
    let separator = l.pick(", ", "、");
    for (variant, tools) in variants_in_catalog() {
        let when = notes
            .variants
            .get(variant.id())
            .unwrap_or_else(|| panic!("template_notes.json has no note for variant {}", variant.id()))
            .get(l);
        let id = json_string(variant.id());
        let tools = tools.join(separator);
        let colon = l.pick(": ", "：");
        comment(&mut out, "  ", &format!("                 {id:<12} {tools}{colon}{when}"));
    }
    for line in [
        l.pick("  description  the tool's description, replacing the built-in one.", "  description  工具描述，替换内置的那段。"),
        l.pick(
            "A built-in tool's description is also prompts' tool.<tool>[.<variant>].description; an entry",
            "内置工具的描述也就是 prompts 里的 tool.<工具>[.<变体>].description，两处都写时以这里为准。",
        ),
        l.pick(
            "here wins over it. An MCP tool's description can be changed only here.",
            "MCP 工具的描述只能在这里改。",
        ),
    ] {
        comment(&mut out, "  ", line);
    }
    out.push_str("  \"tools\": [\n");
    for example in [
        l.pick(
            "{ \"toolName\": \"grep\", \"description\": \"…\" },",
            "{ \"toolName\": \"grep\", \"description\": \"……\" },",
        ),
        l.pick(
            "{ \"toolName\": \"bash\", \"variant\": \"child\", \"description\": \"…\" },",
            "{ \"toolName\": \"bash\", \"variant\": \"child\", \"description\": \"……\" },",
        ),
    ] {
        comment(&mut out, "    ", example);
    }
    out.push_str("  ],\n\n");

    comment(&mut out, "  ", l.pick(
        "Every fixed text Mewrk injects, one key each.",
        "Mewrk 注入的每一段固定文字，一个键一段。",
    ));
    out.push_str("  \"prompts\": {\n");
    let keys = PromptKey::ALL;
    for (index, key) in keys.iter().enumerate() {
        let id = key.id();
        if let Some(heading) = sections.get(id) {
            if index > 0 {
                out.push('\n');
            }
            let heading = match l {
                Language::English => (*heading).to_owned(),
                Language::Chinese => notes
                    .sections
                    .get(*heading)
                    .unwrap_or_else(|| panic!("template_notes.json has no Chinese heading for {heading}"))
                    .clone(),
            };
            comment(&mut out, "    ", &format!("──── {heading} ────"));
        }
        out.push('\n');
        let note = notes
            .keys
            .get(id)
            .unwrap_or_else(|| panic!("template_notes.json has no note for {id}"));
        comment(&mut out, "    ", l.pick(&note.en, &note.zh));
        if !key.placeholders().is_empty() {
            let listed = key
                .placeholders()
                .iter()
                .map(|name| {
                    let meaning = note
                        .placeholders
                        .get(*name)
                        .unwrap_or_else(|| panic!("template_notes.json: {id} has no meaning for {{{name}}}"))
                        .get(l);
                    format!("{{{name}}} {meaning}")
                })
                .collect::<Vec<_>>()
                .join(l.pick("; ", "；"));
            comment(&mut out, "    ", &format!("{}{listed}", l.pick("Placeholders: ", "占位符：")));
        }
        let comma = if index + 1 < keys.len() { "," } else { "" };
        out.push_str(&format!(
            "    {}: {}{comma}\n",
            json_string(id),
            json_string(key.builtin_text())
        ));
    }
    out.push_str("  }\n}\n");
    out
}

impl Language {
    fn pick_lines(self, en: &'static [&'static str], zh: &'static [&'static str]) -> &'static [&'static str] {
        match self {
            Language::English => en,
            Language::Chinese => zh,
        }
    }
}

mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::ResolvedLanguage;
    use crate::prompt_profile::{parse_prompt_overrides, PromptProfile};

    /// Every key has a note, every note names a key, and each note explains
    /// exactly the placeholders its key declares. A note is one line: it sits
    /// in a `//` comment.
    #[test]
    fn the_notes_cover_every_key_and_its_placeholders() {
        let notes = notes();
        let registry = PromptKey::ALL.iter().map(|key| key.id()).collect::<BTreeSet<_>>();
        let noted = notes.keys.keys().map(String::as_str).collect::<BTreeSet<_>>();
        assert_eq!(noted, registry, "template_notes.json and the registry disagree on the keys");
        for key in PromptKey::ALL {
            let note = &notes.keys[key.id()];
            let declared = key.placeholders().iter().copied().collect::<BTreeSet<_>>();
            let explained = note.placeholders.keys().map(String::as_str).collect::<BTreeSet<_>>();
            assert_eq!(explained, declared, "{}: placeholder notes", key.id());
            let texts = [&note.en, &note.zh]
                .into_iter()
                .chain(note.placeholders.values().flat_map(|meaning| [&meaning.en, &meaning.zh]));
            for text in texts {
                assert!(!text.trim().is_empty(), "{}: an empty note", key.id());
                assert!(!text.contains('\n'), "{}: a note spans lines", key.id());
            }
        }
        for (variant, _) in variants_in_catalog() {
            assert!(notes.variants.contains_key(variant.id()), "no note for variant {}", variant.id());
        }
        let headings = section_starts();
        for heading in headings.values() {
            assert!(notes.sections.contains_key(*heading), "no Chinese heading for {heading}");
        }
        assert_eq!(
            headings.values().collect::<BTreeSet<_>>().len(),
            notes.sections.len(),
            "template_notes.json names a section the registry does not have"
        );
    }

    /// A template, copied as it is, is a tool-description file that says
    /// exactly what the built-in says: it parses as JSONC, overrides every key
    /// with the built-in text and describes no tool.
    #[test]
    fn a_template_reads_back_as_the_builtin() {
        for language in Language::ALL {
            let text = render(language);
            let value = crate::config_file::parse_jsonc(text.as_bytes())
                .unwrap_or_else(|| panic!("{} is not JSONC", language.file_name()));
            assert!(serde_json::from_str::<serde_json::Value>(&text).is_err(), "the template has comments");
            let overrides = parse_prompt_overrides(&value);
            assert_eq!(overrides.len(), PromptKey::ALL.len());
            let tools = crate::capabilities::parse_tool_description_entries(&value);
            assert!(tools.is_empty());
            let profile = PromptProfile::from_file(
                "template".into(),
                value["name"].as_str().unwrap().into(),
                ResolvedLanguage::EnUs,
                overrides,
                tools,
            );
            let builtin = PromptProfile::builtin();
            for key in PromptKey::ALL {
                assert_eq!(profile.text(*key), builtin.text(*key), "{}", key.id());
            }
        }
    }
}
