//! Which form of each built-in tool a run offers: its variant.
//!
//! A run property that changes what a built-in tool *is* to the model — what a
//! call does, what comes back, what the model must do first — makes it a
//! different tool, not the same tool with a sentence appended. `edit` under the
//! file write guards refuses a file it has not read; `edit` without them never
//! does, and the two are described as two tools.
//!
//! Each such property is an [`Axis`]. A tool depends on at most one, and every
//! value of that axis is a [`ToolVariant`] with a branch of its own in
//! `builtin_schemas::builtin_tool_schema` and description keys of its own in
//! the prompt profile: `tool.<tool>.description` for the standard variant,
//! `tool.<tool>.<variant>.description` (and `….<variant>.param.<path>` where a
//! parameter's meaning changes too) for the others. A tool-description file
//! names a variant with `tools[].variant`, so it carries one description per
//! variant rather than one text that hedges across all of them.
//!
//! [`ToolSurface`] is a run's value on every axis, read once from the request
//! ([`ToolSurface::of`]). Nothing downstream decides a variant on its own: the
//! step builder, the role injection and `tool_search` all ask
//! [`ToolSurface::variant_of`].
//!
//! What is *not* a variant:
//! - Run data a schema carries in slots of one shape: the workspace numbers of
//!   a multi-workspace run, the configured role names, the skills.
//! - What decides whether a tool is offered at all: plan mode, the memory
//!   tiers, a model without vision for the preview image tools, the shells a
//!   run's machines have. Those add or remove tools
//!   (`aisdk::tools::enabled_tools`, `lib.rs::trusted_run_request`).
//! - What the host says between rounds. A mechanism that is switched off
//!   simply sends nothing; its texts are not rewritten for its absence.

use crate::model::{RunModelRequest, SearchBackend};

/// One run property that changes what the tools depending on it are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Axis {
    /// The conversation's file write guards
    /// (`ConversationSettings::file_write_guards_enabled`): while they are on,
    /// `edit` and `write` refuse an existing file the conversation has not
    /// read, or one that changed on disk since.
    FileWriteGuards,
    /// A model that takes asynchronous tool calls
    /// (`async_tools::takes_async_tools`): `agent_spawn` and `workflow` are
    /// declared asynchronous, and the task's result arrives later as the
    /// call's own output instead of a dispatch receipt now.
    AsyncResults,
    /// A child agent's run (`subagent_depth > 0`): its background commands
    /// end with its final reply, nothing wakes it once it has given one, and
    /// it has no children or workflows of its own to wait for.
    ChildAgent,
    /// What answers `web_search`: the conversation's own model, whose result
    /// is a written report with the sites it consulted, or a search provider,
    /// whose result is a list of citable entries.
    WebSearchBackend,
    /// Whether the model takes images: only one that does is shown the image
    /// `read` opens.
    Vision,
}

impl Axis {
    /// Every axis, in documentation order.
    #[cfg(test)]
    pub(crate) const ALL: &'static [Axis] = &[
        Axis::FileWriteGuards,
        Axis::AsyncResults,
        Axis::ChildAgent,
        Axis::WebSearchBackend,
        Axis::Vision,
    ];

    /// The axis the built-in tool `tool` depends on, if any.
    pub(crate) fn of_tool(tool: &str) -> Option<Self> {
        match tool {
            "edit" | "write" => Some(Self::FileWriteGuards),
            "agent_spawn" | "workflow" => Some(Self::AsyncResults),
            "task_wait" => Some(Self::ChildAgent),
            name if crate::shell_backend::ShellBackend::of_tool(name).is_some() => {
                Some(Self::ChildAgent)
            }
            "web_search" => Some(Self::WebSearchBackend),
            "read" => Some(Self::Vision),
            _ => None,
        }
    }

    /// This axis's variants, the standard one first.
    pub(crate) fn variants(self) -> &'static [ToolVariant] {
        match self {
            Self::FileWriteGuards => &[ToolVariant::Standard, ToolVariant::Unguarded],
            Self::AsyncResults => &[ToolVariant::Standard, ToolVariant::Async],
            Self::ChildAgent => &[ToolVariant::Standard, ToolVariant::Child],
            Self::WebSearchBackend => &[ToolVariant::Standard, ToolVariant::Native],
            Self::Vision => &[ToolVariant::Standard, ToolVariant::TextOnly],
        }
    }
}

/// One form of a built-in tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolVariant {
    /// The form a tool has when nothing in the run changes it: the guards on,
    /// synchronous dispatch, a top-level run, a search provider, a model that
    /// sees images. Every tool has it, and a tool without an axis has only it.
    Standard,
    /// `edit` and `write` with the file write guards off.
    Unguarded,
    /// `agent_spawn` and `workflow` on a model that takes asynchronous calls.
    Async,
    /// The shell command tools and `task_wait` in a child agent.
    Child,
    /// `web_search` answered by the conversation's own model.
    Native,
    /// `read` for a model that does not take images.
    TextOnly,
}

impl ToolVariant {
    /// The id a tool-description file spells the variant with
    /// (`tools[].variant`), and the segment it adds to a key id. Production
    /// only ever parses one ([`ToolVariant::parse`]); the golden baseline and
    /// the tests spell them.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn id(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Unguarded => "unguarded",
            Self::Async => "async",
            Self::Child => "child",
            Self::Native => "native",
            Self::TextOnly => "text_only",
        }
    }

    /// The variant a file names. An absent or empty `variant` is the standard
    /// one; an id this build does not know is `None`, and the entry is
    /// skipped rather than read as some other variant.
    pub fn parse(id: &str) -> Option<Self> {
        match id.trim() {
            "" | "standard" => Some(Self::Standard),
            "unguarded" => Some(Self::Unguarded),
            "async" => Some(Self::Async),
            "child" => Some(Self::Child),
            "native" => Some(Self::Native),
            "text_only" => Some(Self::TextOnly),
            _ => None,
        }
    }
}

/// The variants of the built-in tool `tool`, the standard one first.
pub(crate) fn variants_of(tool: &str) -> &'static [ToolVariant] {
    Axis::of_tool(tool).map_or(&[ToolVariant::Standard], Axis::variants)
}

/// A run's value on every [`Axis`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolSurface {
    pub file_write_guards: bool,
    pub async_results: bool,
    pub child_agent: bool,
    pub native_web_search: bool,
    pub vision: bool,
}

impl Default for ToolSurface {
    /// The surface every tool takes its standard variant on.
    fn default() -> Self {
        Self {
            file_write_guards: true,
            async_results: false,
            child_agent: false,
            native_web_search: false,
            vision: true,
        }
    }
}

impl ToolSurface {
    /// The surface `request` runs on.
    ///
    /// Every input is fixed for the whole run — the conversation's switch, the
    /// model and its endpoint, the depth, the backend the run resolved — so a
    /// tool keeps one variant from the first round to the last.
    pub(crate) fn of(request: &RunModelRequest) -> Self {
        Self {
            file_write_guards: request.file_guard.enabled,
            async_results: crate::async_tools::takes_async_tools(
                &request.provider,
                &request.model,
            ),
            child_agent: request.subagent_depth > 0,
            native_web_search: matches!(request.web_search.backend, Some(SearchBackend::Native)),
            vision: request.model.supports_vision(),
        }
    }

    /// The variant of the built-in tool `tool` on this surface.
    pub(crate) fn variant_of(&self, tool: &str) -> ToolVariant {
        let Some(axis) = Axis::of_tool(tool) else {
            return ToolVariant::Standard;
        };
        match axis {
            Axis::FileWriteGuards if !self.file_write_guards => ToolVariant::Unguarded,
            Axis::AsyncResults if self.async_results => ToolVariant::Async,
            Axis::ChildAgent if self.child_agent => ToolVariant::Child,
            Axis::WebSearchBackend if self.native_web_search => ToolVariant::Native,
            Axis::Vision if !self.vision => ToolVariant::TextOnly,
            _ => ToolVariant::Standard,
        }
    }

    /// The surface whose value on `axis` selects `variant` and is standard
    /// everywhere else: what the golden baseline renders each variant on.
    #[cfg(test)]
    pub(crate) fn selecting(axis: Axis, variant: ToolVariant) -> Self {
        let standard = variant == ToolVariant::Standard;
        let mut surface = Self::default();
        match axis {
            Axis::FileWriteGuards => surface.file_write_guards = standard,
            Axis::AsyncResults => surface.async_results = !standard,
            Axis::ChildAgent => surface.child_agent = !standard,
            Axis::WebSearchBackend => surface.native_web_search = !standard,
            Axis::Vision => surface.vision = standard,
        }
        surface
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_id_round_trips_and_an_unknown_one_is_refused() {
        for axis in Axis::ALL {
            for variant in axis.variants() {
                assert_eq!(ToolVariant::parse(variant.id()), Some(*variant));
            }
        }
        assert_eq!(ToolVariant::parse(""), Some(ToolVariant::Standard));
        assert_eq!(ToolVariant::parse("guarded"), None);
    }

    #[test]
    fn each_axis_starts_with_the_standard_variant_and_selects_each_of_its_own() {
        let catalog = crate::catalog::tool_catalog();
        for axis in Axis::ALL {
            let variants = axis.variants();
            assert_eq!(variants[0], ToolVariant::Standard, "{axis:?}");
            assert!(variants.len() >= 2, "{axis:?} has nothing to choose between");
            let tool = catalog
                .iter()
                .map(|tool| tool.name.as_str())
                .find(|name| Axis::of_tool(name) == Some(*axis))
                .unwrap_or_else(|| panic!("no catalog tool depends on {axis:?}"));
            for variant in variants {
                assert_eq!(
                    ToolSurface::selecting(*axis, *variant).variant_of(tool),
                    *variant,
                    "{axis:?} {variant:?}"
                );
            }
        }
    }

    #[test]
    fn the_default_surface_offers_every_tool_in_its_standard_variant() {
        let surface = ToolSurface::default();
        for tool in crate::catalog::tool_catalog() {
            assert_eq!(surface.variant_of(&tool.name), ToolVariant::Standard, "{}", tool.name);
        }
    }

    #[test]
    fn a_tool_without_an_axis_has_only_the_standard_variant() {
        assert_eq!(variants_of("ls"), &[ToolVariant::Standard]);
        assert_eq!(variants_of("edit"), &[ToolVariant::Standard, ToolVariant::Unguarded]);
        assert_eq!(variants_of("bash"), &[ToolVariant::Standard, ToolVariant::Child]);
    }
}
