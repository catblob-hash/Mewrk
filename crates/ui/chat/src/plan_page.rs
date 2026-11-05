//! `components/PlanPage.tsx`: the plan document and, while the exit card is the pending one, the dock that answers it. Approving
//! a plan you cannot see is the failure the co-located dock exists to prevent, so the two stay in one block wherever the plan
//! is shown.

use std::hash::Hash;

use mewrk_client::model::{ConversationPlan, PlanStatus};
use mewrk_client::prelude::*;
use mewrk_ui_common::markdown::{markdown_content, MarkdownOptions};

/// `.plan-page__empty`.
const EMPTY: TypeStyle = TypeStyle { size: 11.0, line: 16.0, weight: Weight::Regular, mono: false, italic: false, serif: false };

/// JavaScript's `\s`: Rust's whitespace plus the byte-order mark.
fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `.` in a JavaScript regular expression stops at these.
fn is_js_line_end(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `/^#{1,3}\s+(.+?)\s*$/u`.
fn heading_of(line: &str) -> Option<String> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=3).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.starts_with(is_js_space) {
        return None;
    }
    let title = rest.trim_matches(is_js_space);
    (!title.is_empty() && !title.contains(is_js_line_end)).then(|| title.to_owned())
}

/// The plan's own first heading, which is the title the model gave it.
pub fn plan_title(markdown: &str) -> Option<String> {
    markdown.split('\n').take(40).find_map(heading_of)
}

/// Where the plan stands, as one word. A pending exit card outranks the stored status: the document is already written, and what
/// the user is being asked is whether it stands.
pub fn plan_status_label(plan: Option<&ConversationPlan>, awaiting_approval: bool) -> &'static str {
    if awaiting_approval {
        return t("待批准", "Awaiting approval");
    }
    match plan.map(|plan| plan.status) {
        Some(PlanStatus::Approved) => t("已批准", "Approved"),
        // "Rejected" is the stored word for a plan the user sent feedback on.
        Some(PlanStatus::Rejected) => t("待修改", "Changes requested"),
        _ => t("撰写中", "Drafting"),
    }
}

pub fn plan_pane(key: impl Hash, plan: Option<&ConversationPlan>, single_dollar_math: bool, dock: Option<E>) -> E {
    let t_ = theme();
    let k = hash_key(key);
    let markdown = plan.map_or("", |plan| plan.markdown.as_str());
    let body: E = if markdown.trim().is_empty() {
        text(t("模型还没有写下计划。", "The model has not written a plan yet.")).ty(EMPTY).color(t_.text_tertiary)
    } else {
        markdown_content(("plan-markdown", k), markdown, MarkdownOptions { render_html: true, single_dollar_math, ..MarkdownOptions::default() })
    };
    col()
        .key(("plan-page", k))
        .h_full()
        .min_h(0.0)
        .child(div().key(("plan-page-body", k)).flex1().min_h(0.0).scroll_y().pt(16.0).px(22.0).pb(20.0).child(body))
        .child(dock.map(|dock| div().key(("plan-page-dock", k)).px(16.0).pb(14.0).shrink0().border_top(t_.card_stroke).child(dock)))
}
