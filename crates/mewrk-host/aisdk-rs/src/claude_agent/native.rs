//! The admission and record of a native-search or native-fetch session (`NativeLedger`).
//!
//! The only built-in tools the CLI is ever given are its own `WebSearch` and `WebFetch`, one at a time, and only in the
//! isolated one-shot requests the host mints. The permission callback is the one place the CLI asks before using a
//! tool, so that is where the host's budget and URL list are held; a session also makes the CLI ask about every call to
//! its tool (`native_settings`): left to itself `WebFetch` lets a host on its preapproved list through unasked.

use std::collections::{HashMap, HashSet};

use crate::json::Json;
use crate::protocol::MAX_STREAM_TEXT;
use crate::step::{js_len, js_slice};
use crate::{jarr, jobj};

use super::url;

/// The CLI's built-in search tool, switched on only for native-search sessions.
const CLI_SEARCH_TOOL: &str = "WebSearch";
/// Retained sources per search step; the same cap as `main.ts`'s `MAX_SOURCES`.
const MAX_SEARCH_SOURCES: usize = 64;
/// What a `WebSearch` call past the host's budget is answered with.
const SEARCH_BUDGET_SPENT: &str = "The search budget for this query is spent. Answer from the results you already have.";
/// The CLI's built-in page fetch, switched on only for native-fetch sessions.
const CLI_FETCH_TOOL: &str = "WebFetch";
/// Retained pages per fetch step; the same cap as `main.ts`'s `MAX_SOURCES`.
const MAX_FETCH_DOCUMENTS: usize = 64;
/// Longest failure text kept per fetch, so one error page cannot crowd the report.
const MAX_FETCH_FAILURE_CHARS: usize = 1000;
/// The `prompt` every admitted `WebFetch` call runs with, in place of whatever the executor wrote. `WebFetch` does not
/// return the page: the CLI converts it to Markdown and hands that, with this prompt, to a model of its own, whose reply
/// is the call's `result`. This asks for the most faithful rendering the CLI's own instructions allow.
const FETCH_PROMPT: &str = "Reproduce this page's main content as completely and faithfully as you can, as Markdown: all of its text, in its original wording, language and order, keeping headings, lists, tables, code blocks and link targets. Leave out only site navigation, ads, cookie notices and similar page chrome. Do not summarize, shorten, translate, explain or comment; where something cannot be reproduced, say what was left out instead of paraphrasing it.";
/// What a `WebFetch` call for an address the host did not name is answered with.
const FETCH_NOT_ASKED: &str = "This request may fetch only the URLs it was given, exactly as written:";
/// What a second `WebFetch` call for an address already fetched is answered with.
const FETCH_ALREADY_DONE: &str = "This URL was already fetched in this request; its result is above. Do not fetch it again.";
/// What a `WebFetch` call past the host's budget is answered with.
const FETCH_BUDGET_SPENT: &str = "The fetch budget for this request is spent; no further pages may be fetched.";

/// A `PermissionResult` as the control protocol carries it, before `toolUseID` joins.
pub(super) enum Permission {
    Allow(Json),
    Deny(String),
}

pub(super) enum NativeLedger {
    Search(NativeSearchLedger),
    Fetch(NativeFetchLedger),
}

impl NativeLedger {
    /// `nativeLedger`: the ledger a native request's session keeps. The host never sends both.
    pub fn of_request(request: &Json) -> Option<NativeLedger> {
        if let Some(search) = request.non_null("nativeSearch") {
            return Some(NativeLedger::Search(NativeSearchLedger::new(max_uses(search))));
        }
        let fetch = request.non_null("nativeFetch")?;
        let urls: Vec<&str> = fetch.get("urls").and_then(Json::as_arr).map_or(Vec::new(), |urls| urls.iter().filter_map(Json::as_str).collect());
        Some(NativeLedger::Fetch(NativeFetchLedger::new(max_uses(fetch), &urls)))
    }

    pub fn tool(&self) -> &'static str {
        match self {
            NativeLedger::Search(_) => CLI_SEARCH_TOOL,
            NativeLedger::Fetch(_) => CLI_FETCH_TOOL,
        }
    }

    /// The permission callback's answer for one call to `tool`.
    pub fn admit(&mut self, tool_use_id: &str, input: &Json) -> Permission {
        match self {
            NativeLedger::Search(ledger) => ledger.admit(tool_use_id, input),
            NativeLedger::Fetch(ledger) => ledger.admit(tool_use_id, input),
        }
    }

    /// Reads what an admitted call returned, from the user message carrying its result.
    pub fn observe(&mut self, message: &Json) {
        match self {
            NativeLedger::Search(ledger) => ledger.observe(message),
            NativeLedger::Fetch(ledger) => ledger.observe(message),
        }
    }

    /// The step result's fields this session fills, in the order the spread lands them.
    pub fn report(&self) -> Vec<(&'static str, Json)> {
        match self {
            NativeLedger::Search(ledger) => ledger.report(),
            NativeLedger::Fetch(ledger) => ledger.report(),
        }
    }
}

fn max_uses(native: &Json) -> f64 {
    native.get("maxUses").and_then(Json::as_f64).unwrap_or(0.0)
}

/// Budget and record of a native-search session's `WebSearch` calls.
///
/// One use is one `WebSearch` call the CLI was allowed to run: that is what the permission callback can count and
/// refuse, and its tool_use id is the call id the host's budget accounting dedupes on. Sources are read off the call's
/// structured output (`tool_use_result`), not its tool_result text: the titles and URLs there are the hits the
/// server-side search returned, the part of a server-side search the host can check.
pub(super) struct NativeSearchLedger {
    /// The host's remaining budget; zero means unlimited.
    max_uses: f64,
    admitted: Vec<String>,
    found: Vec<Json>,
    found_urls: HashSet<String>,
}

impl NativeSearchLedger {
    fn new(max_uses: f64) -> Self {
        Self { max_uses, admitted: Vec::new(), found: Vec::new(), found_urls: HashSet::new() }
    }

    fn admit(&mut self, tool_use_id: &str, input: &Json) -> Permission {
        if self.max_uses > 0.0 && self.admitted.len() as f64 >= self.max_uses {
            return Permission::Deny(SEARCH_BUDGET_SPENT.to_owned());
        }
        if !self.admitted.iter().any(|id| id == tool_use_id) {
            self.admitted.push(tool_use_id.to_owned());
        }
        Permission::Allow(input.clone())
    }

    fn observe(&mut self, message: &Json) {
        let Some(content) = message.path(&["message", "content"]).and_then(Json::as_arr) else { return };
        let answers = content.iter().any(|block| {
            block.str_at("type") == Some("tool_result")
                && block.str_at("tool_use_id").is_some_and(|id| self.admitted.iter().any(|admitted| admitted == id))
        });
        let Some(results) = message.get("tool_use_result").filter(|output| output.is_obj()).and_then(|output| output.get("results")).and_then(Json::as_arr) else { return };
        if !answers {
            return;
        }
        for entry in results {
            let Some(hits) = entry.get("content").filter(|_| entry.is_obj()).and_then(Json::as_arr) else { continue };
            for hit in hits {
                if hit.is_obj() {
                    self.add_source(hit.get("url"), hit.get("title"));
                }
            }
        }
    }

    fn add_source(&mut self, url_value: Option<&Json>, title: Option<&Json>) {
        if self.found.len() >= MAX_SEARCH_SOURCES {
            return;
        }
        let Some(address) = url_value.and_then(Json::as_str) else { return };
        if js_len(address) > 2048 || self.found_urls.contains(address) {
            return;
        }
        // Search output is untrusted; a malformed URL is not a source.
        match url::parse(address) {
            Some(parsed) if parsed.scheme == "http" || parsed.scheme == "https" => {}
            _ => return,
        }
        self.found_urls.insert(address.to_owned());
        self.found.push(jobj! {
            "id": format!("search:{address}"),
            "url": address,
            "title"?: title.and_then(Json::as_str).filter(|title| !title.is_empty()).map(|title| js_slice(title, 512)),
        });
    }

    fn report(&self) -> Vec<(&'static str, Json)> {
        vec![
            ("nativeSearchUses", Json::from(self.admitted.len())),
            ("nativeSearchCallIds", Json::Arr(self.admitted.iter().map(Json::from).collect())),
            ("sources", Json::Arr(self.found.clone())),
        ]
    }
}

/// `fetchKey`: the address a fetch is compared by, or `None` for one that cannot be fetched at all. Parsing normalizes
/// spelling; the fragment is dropped because it never reaches the server; and `http:` reads as `https:` because
/// `WebFetch` upgrades every plain-HTTP address before fetching it, so the two name the same request.
fn fetch_key(value: Option<&Json>) -> Option<String> {
    let text = value?.as_str()?;
    if text.is_empty() || js_len(text) > 8192 {
        return None;
    }
    let mut parsed = url::parse(js_trim(text))?;
    match parsed.scheme.as_str() {
        "http" => parsed.set_https(),
        "https" => {}
        _ => return None,
    }
    Some(parsed.href())
}

/// `toolResultText`: a tool_result block's text, whether it came as a string or as text blocks.
fn tool_result_text(block: &Json) -> String {
    match block.get("content") {
        Some(Json::Str(text)) => text.clone(),
        Some(Json::Arr(parts)) => parts
            .iter()
            .filter(|part| part.str_at("type") == Some("text"))
            .filter_map(|part| part.str_at("text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `String.prototype.trim`.
pub(super) fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{FEFF}')
}

/// Admission and record of a native-fetch session's `WebFetch` calls.
///
/// The host names the pages and the permission callback holds the CLI to them: a call is admitted only for one of those
/// addresses, only once per address, and only within `maxUses`; anything else is refused with a message saying why,
/// before the CLI opens anything. An admitted call runs with the host's own spelling of the address and with
/// `FETCH_PROMPT` in place of the executor's prompt. A request that names no pages fetches nothing.
///
/// Each admitted call's outcome is read off its structured output into the same shape the Messages families'
/// `web_fetch_result` becomes in `main.ts`: a 2xx answer with text is a page, anything else a `providerToolErrors` entry
/// under the host's own tool name, `web_fetch`.
pub(super) struct NativeFetchLedger {
    /// `max_uses` is the host's budget, one per page; zero leaves only the list to bound it.
    max_uses: f64,
    /// `fetchKey` of each page the host asked for, in order, with the host's spelling of it.
    asked: Vec<(String, String)>,
    /// Admitted tool_use id to the page it fetches, in the host's spelling.
    admitted: HashMap<String, String>,
    /// Keys already admitted once.
    opened: HashSet<String>,
    /// Admitted calls whose outcome has been read.
    settled: HashSet<String>,
    documents: Vec<Json>,
    failures: Vec<Json>,
}

impl NativeFetchLedger {
    fn new(max_uses: f64, urls: &[&str]) -> Self {
        let mut asked: Vec<(String, String)> = Vec::new();
        for page in urls {
            if let Some(key) = fetch_key(Some(&Json::from(*page))) {
                if !asked.iter().any(|(known, _)| *known == key) {
                    asked.push((key, (*page).to_owned()));
                }
            }
        }
        Self {
            max_uses,
            asked,
            admitted: HashMap::new(),
            opened: HashSet::new(),
            settled: HashSet::new(),
            documents: Vec::new(),
            failures: Vec::new(),
        }
    }

    /// `{ ...input, url, prompt: FETCH_PROMPT }`.
    fn rewritten(input: &Json, page: &str) -> Json {
        let mut object = input.as_obj().cloned().unwrap_or_default();
        object.insert("url", page);
        object.insert("prompt", FETCH_PROMPT);
        Json::Obj(object)
    }

    fn admit(&mut self, tool_use_id: &str, input: &Json) -> Permission {
        if let Some(already) = self.admitted.get(tool_use_id) {
            return Permission::Allow(Self::rewritten(input, already));
        }
        let key = fetch_key(input.get("url"));
        let page = key.as_ref().and_then(|key| self.asked.iter().find(|(known, _)| known == key)).map(|(_, page)| page.clone());
        let (Some(key), Some(page)) = (key, page) else {
            let listed: Vec<&str> = self.asked.iter().map(|(_, page)| page.as_str()).collect();
            let shown = if listed.is_empty() { "(none)".to_owned() } else { listed.join("\n") };
            return Permission::Deny(format!("{FETCH_NOT_ASKED}\n{shown}"));
        };
        if self.opened.contains(&key) {
            return Permission::Deny(FETCH_ALREADY_DONE.to_owned());
        }
        if self.max_uses > 0.0 && self.admitted.len() as f64 >= self.max_uses {
            return Permission::Deny(FETCH_BUDGET_SPENT.to_owned());
        }
        self.opened.insert(key);
        self.admitted.insert(tool_use_id.to_owned(), page.clone());
        Permission::Allow(Self::rewritten(input, &page))
    }

    fn observe(&mut self, message: &Json) {
        let Some(content) = message.path(&["message", "content"]).and_then(Json::as_arr) else { return };
        let answers: Vec<&Json> = content
            .iter()
            .filter(|block| {
                block.str_at("type") == Some("tool_result")
                    && block.str_at("tool_use_id").is_some_and(|id| self.admitted.contains_key(id) && !self.settled.contains(id))
            })
            .collect();
        // `tool_use_result` is the structured output of the message's one result; a message answering several calls at
        // once is read from their text alone.
        let structured = if answers.len() == 1 { message.get("tool_use_result") } else { None };
        for block in answers {
            self.record(block, structured);
        }
    }

    fn record(&mut self, block: &Json, output: Option<&Json>) {
        let Some(id) = block.str_at("tool_use_id") else { return };
        let Some(page) = self.admitted.get(id).cloned() else { return };
        self.settled.insert(id.to_owned());
        if block.get("is_error").and_then(Json::as_bool) == Some(true) {
            let cleaned = tool_result_text(block).replace("<tool_use_error>", "").replace("</tool_use_error>", "");
            let mut detail = js_trim(&cleaned).to_owned();
            if detail.is_empty() {
                detail = output.and_then(Json::as_str).map(|text| js_trim(text).to_owned()).unwrap_or_default();
            }
            self.fail(&page, if detail.is_empty() { "Claude Code's WebFetch failed" } else { &detail });
            return;
        }
        let fetched = output.filter(|value| value.is_obj());
        let code = fetched.and_then(|value| value.get("code")).and_then(Json::as_f64).filter(|code| code.is_finite());
        let text = fetched.and_then(|value| value.str_at("result")).map_or_else(|| tool_result_text(block), str::to_owned);
        if let Some(code) = code.filter(|code| *code < 200.0 || *code > 299.0) {
            let reason = fetched.and_then(|value| value.str_at("codeText")).map_or("", js_trim);
            let number = Json::from(code).to_text();
            let status = if reason.is_empty() { format!("HTTP {number}") } else { format!("HTTP {number} {reason}") };
            // A redirect's text names its target; keep it, so the model can ask again.
            let body = js_trim(&text);
            self.fail(&page, &if body.is_empty() { status } else { format!("{status}: {body}") });
            return;
        }
        if js_trim(&text).is_empty() {
            self.fail(&page, "Claude Code's WebFetch returned no content");
            return;
        }
        if self.documents.len() >= MAX_FETCH_DOCUMENTS {
            return;
        }
        self.documents.push(jobj! { "url": js_slice(&page, 2048), "text": js_slice(&text, MAX_STREAM_TEXT) });
    }

    fn fail(&mut self, page: &str, detail: &str) {
        let message = format!("{page}: {detail}");
        let shown = if js_len(&message) > MAX_FETCH_FAILURE_CHARS {
            format!("{}…", js_slice(&message, MAX_FETCH_FAILURE_CHARS))
        } else {
            message
        };
        self.failures.push(jobj! { "toolName": "web_fetch", "message": shown });
    }

    fn report(&self) -> Vec<(&'static str, Json)> {
        let mut out = Vec::new();
        if !self.documents.is_empty() {
            out.push(("webDocuments", Json::Arr(self.documents.clone())));
        }
        if !self.failures.is_empty() {
            out.push(("providerToolErrors", Json::Arr(self.failures.clone())));
        }
        out
    }
}

/// `nativeSettings`: flag settings that route every call to the session's built-in through the permission callback. An
/// `ask` rule for the whole tool is consulted before a tool's own permission check, so `WebFetch`'s preapproved hosts
/// cannot skip the ledger and neither can anything a later CLI starts allowing on its own.
pub(super) fn native_settings(native: Option<&NativeLedger>) -> Option<Json> {
    native.map(|ledger| jobj! { "ask": jarr![ledger.tool()] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(permission: Permission) -> String {
        match permission {
            Permission::Deny(message) => message,
            Permission::Allow(input) => panic!("allowed: {input}"),
        }
    }

    fn allow(permission: Permission) -> Json {
        match permission {
            Permission::Allow(input) => input,
            Permission::Deny(message) => panic!("denied: {message}"),
        }
    }

    #[test]
    fn a_search_budget_counts_admissions() {
        let mut ledger = NativeSearchLedger::new(1.0);
        let input = jobj! { "query": "q" };
        assert_eq!(allow(ledger.admit("a", &input)), input);
        assert_eq!(deny(ledger.admit("b", &input)), SEARCH_BUDGET_SPENT);
        let mut unlimited = NativeSearchLedger::new(0.0);
        for id in ["a", "b", "c"] {
            allow(unlimited.admit(id, &input));
        }
        assert_eq!(unlimited.report()[0].1, Json::from(3usize));
    }

    #[test]
    fn search_sources_come_from_structured_output_of_admitted_calls() {
        let mut ledger = NativeSearchLedger::new(0.0);
        allow(ledger.admit("t1", &Json::obj()));
        let message = Json::parse(
            r#"{"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"text"}]},
            "tool_use_result":{"results":[{"content":[
                {"title":"A","url":"https://a.example/x"},
                {"title":"dup","url":"https://a.example/x"},
                {"title":"","url":"http://b.example/"},
                {"title":"bad","url":"not a url"},
                {"title":"ftp","url":"ftp://c.example/"}
            ]},"just text"]}}"#,
        )
        .unwrap();
        ledger.observe(&message);
        assert_eq!(
            ledger.report()[2].1.to_text(),
            r#"[{"id":"search:https://a.example/x","url":"https://a.example/x","title":"A"},{"id":"search:http://b.example/","url":"http://b.example/"}]"#
        );
        // A message answering a call nobody admitted reads nothing.
        let mut other = NativeSearchLedger::new(0.0);
        other.observe(&message);
        assert_eq!(other.report()[2].1.to_text(), "[]");
    }

    #[test]
    fn a_fetch_is_held_to_the_named_pages() {
        let mut ledger = NativeFetchLedger::new(2.0, &["https://example.com/a", "https://example.com/b", "https://example.com/c"]);
        let call = |url: &str| jobj! { "url": url, "prompt": "mine", "extra": 1u64 };
        let admitted = allow(ledger.admit("t1", &call("http://EXAMPLE.com/a#x")));
        assert_eq!(admitted.str_at("url"), Some("https://example.com/a"));
        assert_eq!(admitted.str_at("prompt"), Some(FETCH_PROMPT));
        assert_eq!(admitted.as_obj().unwrap().keys().collect::<Vec<_>>(), ["url", "prompt", "extra"]);
        // The same call asked about again keeps its answer; another call for the page is refused.
        assert_eq!(allow(ledger.admit("t1", &call("whatever"))).str_at("url"), Some("https://example.com/a"));
        assert_eq!(deny(ledger.admit("t2", &call("https://example.com/a"))), FETCH_ALREADY_DONE);
        assert!(deny(ledger.admit("t3", &call("https://docs.python.org/3/"))).contains("https://example.com/b"));
        assert!(deny(ledger.admit("t3", &Json::obj())).starts_with(FETCH_NOT_ASKED));
        allow(ledger.admit("t4", &call("https://example.com/b")));
        assert_eq!(deny(ledger.admit("t5", &call("https://example.com/c"))), FETCH_BUDGET_SPENT);
        assert!(deny(NativeFetchLedger::new(0.0, &[]).admit("t", &call("https://x/"))).ends_with("(none)"));
    }

    #[test]
    fn fetch_outcomes_become_documents_and_failures() {
        let urls = ["https://e.com/ok", "https://e.com/gone", "https://e.com/err", "https://e.com/empty", "https://e.com/moved"];
        let mut ledger = NativeFetchLedger::new(0.0, &urls);
        for (index, page) in urls.iter().enumerate() {
            allow(ledger.admit(&format!("t{index}"), &jobj! { "url": *page }));
        }
        let observe = |ledger: &mut NativeFetchLedger, id: &str, extra: &str, output: &str| {
            let text = format!(r#"{{"message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}"{extra}}}]}},"tool_use_result":{output}}}"#);
            ledger.observe(&Json::parse(&text).unwrap());
        };
        observe(&mut ledger, "t0", r#","content":"from text""#, r#"{"code":200,"codeText":"OK","result":"Page"}"#);
        observe(&mut ledger, "t1", r#","content":"x""#, r#"{"code":404,"codeText":"Not Found","result":""}"#);
        observe(&mut ledger, "t2", r#","is_error":true,"content":"<tool_use_error>boom</tool_use_error>""#, r#""ignored""#);
        observe(&mut ledger, "t3", r#","content":"  ""#, r#"{"code":200,"result":" "}"#);
        observe(&mut ledger, "t4", r#","content":"x""#, r#"{"code":301,"codeText":"Moved","result":"see https://else/"}"#);
        let report = ledger.report();
        assert_eq!(report[0].1.to_text(), r#"[{"url":"https://e.com/ok","text":"Page"}]"#);
        let messages: Vec<String> = report[1].1.as_arr().unwrap().iter().map(|f| f.str_at("message").unwrap().to_owned()).collect();
        assert_eq!(
            messages,
            [
                "https://e.com/gone: HTTP 404 Not Found",
                "https://e.com/err: boom",
                "https://e.com/empty: Claude Code's WebFetch returned no content",
                "https://e.com/moved: HTTP 301 Moved: see https://else/",
            ]
        );
    }

    #[test]
    fn a_long_failure_is_cut() {
        let mut ledger = NativeFetchLedger::new(0.0, &["https://e.com/a"]);
        ledger.fail("https://e.com/a", &"x".repeat(2000));
        let message = ledger.failures[0].str_at("message").unwrap().to_owned();
        assert_eq!(js_len(&message), MAX_FETCH_FAILURE_CHARS + 1);
        assert!(message.ends_with('…'));
    }
}
