//! What a host text says about *other* tools, kept only while they are offered.
//!
//! A built-in tool's description often points at a sibling: the shell tools
//! send file work to `read` and `grep`, `find` contrasts itself with `grep`,
//! `web_fetch` sends a topic to `web_search` first. Which siblings a run
//! offers is the conversation's choice — the user can turn any of them off,
//! and plan mode, the handoff and a child agent's role add and remove tools —
//! so a sentence that names one is true in some runs and false in others.
//!
//! The sibling set is run data, not a variant (`crate::tool_surface`): it
//! never changes what the describing tool *does*, only which of its sentences
//! hold. So the texts mark those sentences, and the host drops each one whose
//! sibling the run does not offer:
//!
//! - `{?read}…{/}` keeps `…` when `read` is offered; `{?read|grep}…{/}` when
//!   either is.
//! - `{!read}…{/}` keeps `…` when none of the named tools is offered — the
//!   wording for a run without them.
//! - `@shell` names every shell tool at once.
//!
//! Segments nest, and `{/}` closes the innermost one. The markers sit in the
//! prompt-profile texts themselves ([`crate::prompt_profile`]), so a
//! tool-description file can use them too; a profile key's own `{name}`
//! placeholders are unaffected, since a marker is never a bare name. A stray
//! `{/}` is dropped and an unclosed segment runs to the end of the text.
//!
//! Marked texts are resolved where a schema is assembled for the wire
//! ([`crate::aisdk::tools::wire_tool_schema`]), against the tools the same
//! request offers ([`OfferedTools::of`]).

use std::borrow::Cow;
use std::collections::BTreeSet;

use serde_json::Value;

/// The group name that stands for every shell tool.
const SHELL_GROUP: &str = "@shell";

/// The tools one request offers, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OfferedTools {
    names: BTreeSet<String>,
}

impl OfferedTools {
    /// The tools `request` puts on the wire this step.
    pub(crate) fn of(request: &crate::model::RunModelRequest) -> Self {
        Self::from_names(
            crate::aisdk::tools::enabled_tools(request)
                .into_iter()
                .map(|tool| tool.name.as_str()),
        )
    }

    pub(crate) fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            names: names.into_iter().map(str::to_owned).collect(),
        }
    }

    /// Every tool of the built-in catalog, the host-derived ones included:
    /// what the golden baselines render a text against, so they show each
    /// sentence some run can see.
    #[cfg(test)]
    pub(crate) fn everything() -> Self {
        Self {
            names: crate::catalog::tool_catalog()
                .into_iter()
                .map(|tool| tool.name)
                .collect(),
        }
    }

    /// Whether `name` — a tool, or [`SHELL_GROUP`] — is offered.
    pub(crate) fn has(&self, name: &str) -> bool {
        if name == SHELL_GROUP {
            return self
                .names
                .iter()
                .any(|tool| crate::shell_backend::ShellBackend::of_tool(tool).is_some());
        }
        self.names.contains(name)
    }
}

/// One marker, as the text spells it.
enum Marker<'a> {
    /// `{?names}`: keep while any is offered.
    Any(&'a str),
    /// `{!names}`: keep while none is offered.
    None(&'a str),
    /// `{/}`.
    Close,
}

/// The marker starting at `text[0]`, and its length in bytes.
fn marker_at(text: &str) -> Option<(Marker<'_>, usize)> {
    if text.starts_with("{/}") {
        return Some((Marker::Close, 3));
    }
    let (kind, rest) = match text.as_bytes().get(..2)? {
        b"{?" => (true, &text[2..]),
        b"{!" => (false, &text[2..]),
        _ => return None,
    };
    let close = rest.find('}')?;
    let names = &rest[..close];
    let well_formed = !names.is_empty()
        && names.split('|').all(|name| {
            let name = name.strip_prefix('@').unwrap_or(name);
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        });
    if !well_formed {
        return None;
    }
    let marker = if kind {
        Marker::Any(names)
    } else {
        Marker::None(names)
    };
    Some((marker, 2 + close + 1))
}

/// `text` with every segment whose condition `offered` does not meet removed,
/// and every marker gone. A text without markers comes back borrowed.
pub(crate) fn resolve<'a>(text: &'a str, offered: &OfferedTools) -> Cow<'a, str> {
    if !text.contains("{?") && !text.contains("{!") && !text.contains("{/}") {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    // Whether each open segment is kept; text is written only while all are.
    let mut open: Vec<bool> = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find('{') {
        let shown = open.iter().all(|kept| *kept);
        if shown {
            output.push_str(&rest[..index]);
        }
        let at = &rest[index..];
        match marker_at(at) {
            Some((Marker::Any(names), length)) => {
                open.push(names.split('|').any(|name| offered.has(name)));
                rest = &at[length..];
            }
            Some((Marker::None(names), length)) => {
                open.push(!names.split('|').any(|name| offered.has(name)));
                rest = &at[length..];
            }
            Some((Marker::Close, length)) => {
                open.pop();
                rest = &at[length..];
            }
            None => {
                if shown {
                    output.push('{');
                }
                rest = &at[1..];
            }
        }
    }
    if open.iter().all(|kept| *kept) {
        output.push_str(rest);
    }
    Cow::Owned(output)
}

/// Resolves every `description` string in a built-in tool's schema, root,
/// parameters, items and `$defs` alike. A parameter *named* `description` is
/// an object and is walked into, not rewritten.
pub(crate) fn resolve_schema(schema: &mut Value, offered: &OfferedTools) {
    match schema {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                match value {
                    Value::String(text) if key == "description" => {
                        if let Cow::Owned(resolved) = resolve(text, offered) {
                            *text = resolved;
                        }
                    }
                    _ => resolve_schema(value, offered),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                resolve_schema(item, offered);
            }
        }
        _ => {}
    }
}

/// Every tool name a text's markers mention, for the registry's tests.
#[cfg(test)]
pub(crate) fn mentioned_tools(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find('{') {
        let at = &rest[index..];
        match marker_at(at) {
            Some((Marker::Any(list) | Marker::None(list), length)) => {
                names.extend(list.split('|').map(str::to_owned));
                rest = &at[length..];
            }
            Some((Marker::Close, length)) => rest = &at[length..],
            None => rest = &at[1..],
        }
    }
    names
}

/// Whether a text's markers are balanced: every segment closed, no stray
/// `{/}`.
#[cfg(test)]
pub(crate) fn is_balanced(text: &str) -> bool {
    let mut depth = 0usize;
    let mut rest = text;
    while let Some(index) = rest.find('{') {
        let at = &rest[index..];
        match marker_at(at) {
            Some((Marker::Any(_) | Marker::None(_), length)) => {
                depth += 1;
                rest = &at[length..];
            }
            Some((Marker::Close, length)) => {
                let Some(less) = depth.checked_sub(1) else {
                    return false;
                };
                depth = less;
                rest = &at[length..];
            }
            None => rest = &at[1..],
        }
    }
    depth == 0
}

/// Whether `name` is something a marker may name: a catalog tool or the
/// shell group.
#[cfg(test)]
pub(crate) fn is_known_name(name: &str) -> bool {
    name == SHELL_GROUP || OfferedTools::everything().names.contains(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn offering(names: &[&str]) -> OfferedTools {
        OfferedTools::from_names(names.iter().copied())
    }

    #[test]
    fn a_segment_stays_only_while_its_tool_is_offered() {
        let text = "Lists files.{?grep} Unlike grep, it sees ignored paths.{/} Done.";
        assert_eq!(
            resolve(text, &offering(&["grep"])),
            "Lists files. Unlike grep, it sees ignored paths. Done."
        );
        assert_eq!(resolve(text, &offering(&[])), "Lists files. Done.");
    }

    #[test]
    fn alternatives_negation_and_nesting_compose() {
        let text = "use {?read}read{?grep} or {/}{/}{?grep}grep{/}{!read|grep}nothing{/} here";
        assert_eq!(resolve(text, &offering(&["read", "grep"])), "use read or grep here");
        assert_eq!(resolve(text, &offering(&["read"])), "use read here");
        assert_eq!(resolve(text, &offering(&["grep"])), "use grep here");
        assert_eq!(resolve(text, &offering(&[])), "use nothing here");
    }

    #[test]
    fn the_shell_group_is_any_shell_tool() {
        let text = "{?@shell}a shell command{/}";
        assert_eq!(resolve(text, &offering(&["pwsh"])), "a shell command");
        assert_eq!(resolve(text, &offering(&["sh"])), "a shell command");
        assert_eq!(resolve(text, &offering(&["read"])), "");
    }

    /// Placeholders, JSON examples and stray braces are text, not markers.
    #[test]
    fn braces_that_are_not_markers_pass_through() {
        let text = "{default_ms} ms; {\n  \"version\": 1\n}; {?Bad}x{/} {? read}";
        assert_eq!(
            resolve(text, &offering(&[])),
            "{default_ms} ms; {\n  \"version\": 1\n}; {?Bad}x {? read}"
        );
        let plain = "no markers at all {here}";
        assert!(matches!(resolve(plain, &offering(&[])), Cow::Borrowed(_)));
    }

    #[test]
    fn a_stray_close_is_dropped_and_an_open_segment_runs_to_the_end() {
        assert_eq!(resolve("a{/}b", &offering(&[])), "ab");
        assert_eq!(resolve("a{?grep}b", &offering(&[])), "a");
        assert!(!is_balanced("a{/}b"));
        assert!(!is_balanced("a{?grep}b"));
        assert!(is_balanced("a{?grep}b{!read}c{/}{/}"));
    }

    #[test]
    fn a_schema_resolves_every_description_but_not_a_parameter_named_description() {
        let mut schema = json!({
            "description": "root{?grep} grep{/}",
            "properties": {
                "description": {"type": "string", "description": "param{?grep} grep{/}"},
                "items": {"type": "array", "items": {"description": "{!grep}no grep{/}"}}
            },
            "$defs": {"x": {"description": "{?read}read{/}"}}
        });
        resolve_schema(&mut schema, &offering(&[]));
        assert_eq!(
            schema,
            json!({
                "description": "root",
                "properties": {
                    "description": {"type": "string", "description": "param"},
                    "items": {"type": "array", "items": {"description": "no grep"}}
                },
                "$defs": {"x": {"description": ""}}
            })
        );
    }

    #[test]
    fn marker_names_are_collected() {
        assert_eq!(
            mentioned_tools("{?read|@shell}a{!grep}b{/}{/}{default}"),
            ["read", "@shell", "grep"]
        );
        assert!(is_known_name("grep") && is_known_name("@shell") && is_known_name("skill"));
        assert!(!is_known_name("Bash"));
    }
}
