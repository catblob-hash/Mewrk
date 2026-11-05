//! Upstream's `lib/lineDiff.test.ts`, `lib/pathDisplay.test.ts`, and `rollingSlots` from `RollingNumber.test.tsx`.

use mewrk_pane_tasks::line_diff::{line_diff, line_diff_with, LineDiffResult};
use mewrk_pane_git::parse_unified_diff_lines as parse_unified_diff;
use mewrk_pane_tasks::widgets::{elide_path, rolling_slots, RollSlot};

fn twenty() -> String {
    (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
}

fn hunks(patch: &str) -> Vec<&str> {
    patch.split('\n').filter(|l| l.starts_with("@@")).collect()
}

#[test]
fn identical_strings_make_an_empty_patch() {
    for (a, b) in [("alpha\nbeta\ngamma", "alpha\nbeta\ngamma"), ("alpha\nbeta\n", "alpha\nbeta\n"), ("", ""), ("alpha\nbeta\n", "alpha\nbeta")] {
        assert_eq!(line_diff(a, b, None), LineDiffResult::default());
    }
}

#[test]
fn reports_changes_with_upstreams_hunk_headers() {
    let r = line_diff(&twenty(), &twenty().replace("line 10", "line ten"), None);
    assert_eq!((r.additions, r.deletions, r.bailed), (1, 1, false));
    assert_eq!(hunks(&r.patch), ["@@ -7,7 +7,7 @@"]);
    assert!(r.patch.contains("-line 10") && r.patch.contains("+line ten"));
    let r = line_diff("alpha\nbeta\ngamma", "alpha\nbeta\ninserted\ngamma", None);
    assert_eq!((r.additions, r.deletions), (1, 0));
    assert_eq!(hunks(&r.patch), ["@@ -1,3 +1,4 @@"]);
    let r = line_diff("alpha\nbeta\ngamma", "alpha\ngamma", None);
    assert_eq!((r.additions, r.deletions), (0, 1));
    assert_eq!(hunks(&r.patch), ["@@ -1,3 +1,2 @@"]);
    assert_eq!(hunks(&line_diff("", "alpha\nbeta\ngamma", None).patch), ["@@ -0,0 +1,3 @@"]);
    assert_eq!(hunks(&line_diff("alpha\nbeta\ngamma", "", None).patch), ["@@ -1,3 +0,0 @@"]);
}

#[test]
fn normalises_crlf_and_lone_cr_before_comparing() {
    let (a, b) = (line_diff("alpha\r\nbeta\r\ngamma", "alpha\nline two\ngamma", None), line_diff("alpha\rbeta\rgamma", "alpha\nline two\ngamma", None));
    assert_eq!((a.additions, a.deletions), (1, 1));
    assert_eq!(a.patch, b.patch);
}

#[test]
fn keeps_distant_edits_apart_and_merges_nearby_ones() {
    let r = line_diff(&twenty(), &twenty().replacen("line 2\n", "line two\n", 1).replace("line 19", "line nineteen"), None);
    assert_eq!((r.additions, r.deletions), (2, 2));
    assert_eq!(hunks(&r.patch), ["@@ -1,5 +1,5 @@", "@@ -16,5 +16,5 @@"]);
    let r = line_diff(&twenty(), &twenty().replace("line 10", "line ten").replace("line 12", "line twelve"), None);
    assert_eq!(hunks(&r.patch), ["@@ -7,9 +7,9 @@"]);
    let r = line_diff_with(&twenty(), &twenty().replace("line 10", "line ten"), None, 4000, 1_000_000, 1);
    assert_eq!(hunks(&r.patch), ["@@ -9,3 +9,3 @@"]);
}

#[test]
fn bails_past_its_limits_with_a_whole_rewrite_and_correct_counts() {
    let (a, b) = ("a\nb\nc\nd\ne", "1\n2\n3\n4\n5");
    let r = line_diff_with(a, b, None, 2, 1_000_000, 3);
    assert_eq!((r.bailed, r.additions, r.deletions), (true, 5, 5));
    assert_eq!(hunks(&r.patch), ["@@ -1,5 +1,5 @@"]);
    let r = line_diff_with(a, b, None, 4000, 8, 3);
    assert_eq!((r.bailed, r.additions, r.deletions), (true, 5, 5));
}

#[test]
fn writes_file_headers_when_a_path_is_given_and_omits_them_otherwise() {
    assert!(line_diff("alpha\nbeta", "alpha\ngamma", Some("m.txt")).patch.starts_with("--- a/m.txt\n+++ b/m.txt\n"));
    assert!(line_diff("alpha\nbeta", "alpha\ngamma", None).patch.starts_with("@@ "));
}

#[test]
fn round_trips_through_the_diff_parser_with_matching_counts_and_path() {
    let cases = [("alpha\nbeta\ngamma".to_owned(), "alpha\nbeta\ngamma delta".to_owned()), (twenty(), twenty().replace("line 5", "line five").replace("line 15", "line fifteen")), (String::new(), "brand new\ncontent\nhere".to_owned()), ("old\ncontent\nhere".to_owned(), String::new()), ("keep\n- minus\nplus +\nkeep".to_owned(), "keep\n+++ triple\nkeep".to_owned()), ("a\r\nb\r\nc".to_owned(), "a\nb two\nc".to_owned())];
    for (a, b) in cases {
        let r = line_diff(&a, &b, Some("m.txt"));
        let p = parse_unified_diff(&r.patch);
        assert_eq!((p.additions, p.deletions, p.path.as_deref(), r.bailed), (r.additions, r.deletions, Some("m.txt"), false));
    }
}

fn within(limit: usize) -> impl FnMut(&str) -> bool {
    move |t: &str| t.chars().count() <= limit
}

fn elide(path: &str, limit: usize) -> String {
    elide_path(path, &mut within(limit))
}

#[test]
fn elides_a_path_the_way_upstream_does() {
    let path = "/Users/holycat/Desktop/projects/mewrk";
    assert_eq!(elide(path, 100), path);
    assert_eq!(elide(path, 14), "/Users/…/mewrk");
    assert_eq!(elide(path, 23), "/Users/…/projects/mewrk");
    assert_eq!(elide(path, 30), "/Users/…/projects/mewrk");
    assert_eq!(elide(path, 31), "/Users/holycat/…/projects/mewrk");
    assert_eq!(elide(path, 36), "/Users/holycat/…/projects/mewrk");
    assert_eq!(elide(path, 12), "/U…s/…/mewrk");
    assert_eq!(elide(path, 10), "/U…/…/me…k");
    assert_eq!(elide(path, 2), "/U…/…/m…");
    assert_eq!(elide("C:\\Users\\me\\Documents\\app", 10), "C:\\…\\app");
    assert_eq!(elide("\\\\server\\share\\dir\\sub", 16), "\\\\server\\…\\sub");
    assert_eq!(elide("~/src/app/web", 8), "~/…/web");
    assert_eq!(elide("/srv/www/app/", 11), "/srv/…/app/");
    assert_eq!(elide("/Users/mewrk", 9), "/U…/mewrk");
    assert_eq!(elide("/mewrk", 4), "/m…k");
    assert_eq!(elide("/文档/项目/工作/猫猫项目", 10), "/文档/…/猫猫项目");
}

fn d(from: u8, to: u8) -> RollSlot {
    RollSlot::Digit { from, to }
}

fn g(s: &str) -> RollSlot {
    RollSlot::Glyph(s.to_owned())
}

#[test]
fn pairs_rolling_columns_from_the_right() {
    assert_eq!(rolling_slots("9s", "10s"), [d(1, 1), d(9, 0), g("s")]);
    assert_eq!(rolling_slots("1m23s", "1m24s"), [d(1, 1), g("m"), d(2, 2), d(3, 4), g("s")]);
    assert_eq!(rolling_slots("59s", "1m0s"), [d(1, 1), g("m"), d(9, 0), g("s")]);
    assert_eq!(rolling_slots("1.2k tokens", "1.3k tokens"), [d(1, 1), g("."), d(2, 3), g("k tokens")]);
}
