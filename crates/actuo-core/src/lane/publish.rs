//! What actuo publishes to GitHub: agent Markdown made unable to fetch anything
//! or notify anyone, and the check for credential formats that runs before any
//! commit, push or post.

use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// A credential format found in text: which one, and on which line (from 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub format: &'static str,
    pub line: usize,
}

struct Format {
    name: &'static str,
    re: Regex,
    /// A prefix another format judges (`regex` has no look-ahead).
    not: Option<&'static str>,
}

/// The formats lane processes can reach. A pattern's `r` group is its random part:
/// one with fewer than 8 distinct characters is a placeholder and doesn't count.
static FORMATS: LazyLock<Vec<Format>> = LazyLock::new(|| {
    [
        (
            "an Anthropic key or token",
            r"\bsk-ant-[a-z]+\d{2}-(?P<r>[A-Za-z0-9_-]{40,})",
        ),
        (
            "an OpenAI API key",
            r"\bsk-(?:proj-|svcacct-|admin-)?(?P<r>[A-Za-z0-9_-]{40,})",
        ),
        (
            "a JWT",
            r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.(?P<r>[A-Za-z0-9_-]{20,})",
        ),
        (
            "a Codex refresh token",
            r"\b[a-z]{2}\.\d+\.(?P<r>[A-Za-z0-9_-]{100,})",
        ),
        ("a GitHub token", r"\bgh[pousr]_(?P<r>[A-Za-z0-9]{36})\b"),
        ("a GitHub token", r"\bgithub_pat_(?P<r>[A-Za-z0-9_]{82})"),
        ("an npm token", r"\bnpm_(?P<r>[A-Za-z0-9]{36})\b"),
        (
            "a private key",
            r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
        ),
    ]
    .into_iter()
    .map(|(name, re)| Format {
        name,
        re: Regex::new(re).expect("credential pattern"),
        not: (name == "an OpenAI API key").then_some("sk-ant-"),
    })
    .collect()
});

/// The format named for a value actuo passes to an agent (`pass_env`).
pub const PASSED: &str = "a token or key actuo passes to an agent";
const PASSED_MIN_LEN: usize = 16;

/// The first credential format in `text`, or a value actuo passes to an agent,
/// if any.
pub fn scan(text: &str) -> Option<Hit> {
    let mut first: Option<(usize, &'static str)> = None;
    for f in FORMATS.iter() {
        for c in f.re.captures_iter(text) {
            let whole = c.get(0).expect("whole match");
            if f.not.is_some_and(|p| whole.as_str().starts_with(p)) {
                continue;
            }
            if let Some(r) = c.name("r") {
                let mut seen: Vec<char> = r.as_str().chars().collect();
                seen.sort_unstable();
                seen.dedup();
                if seen.len() < 8 {
                    continue;
                }
            }
            let at = whole.start();
            if first.is_none_or(|(p, _)| at < p) {
                first = Some((at, f.name));
            }
            break;
        }
    }
    // A value actuo passes to an agent, whatever its format. Shorter ones
    // aren't credentials and would match ordinary text.
    for v in crate::pass_env::values().filter(|v| v.len() >= PASSED_MIN_LEN) {
        if let Some(at) = text.find(v)
            && first.is_none_or(|(p, _)| at < p)
        {
            first = Some((at, PASSED));
        }
    }
    first.map(|(at, format)| Hit {
        format,
        line: text[..at].matches('\n').count() + 1,
    })
}

/// Where a credential format is in a diff (`git diff -U0` with `a/` and `b/`
/// prefixes): the file and its line in the new version, or the file name itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHit {
    pub path: String,
    /// `None`: the file's name has it.
    pub line: Option<usize>,
    pub format: &'static str,
}

/// Scans the added lines and the new file names of a diff.
pub fn scan_diff(diff: &str) -> Option<DiffHit> {
    let mut path = String::new();
    let mut line = 0usize;
    for l in diff.lines() {
        let new_name = l
            .strip_prefix("+++ b/")
            .or_else(|| l.strip_prefix("rename to "))
            .or_else(|| l.strip_prefix("copy to "));
        if let Some(name) = new_name {
            path = name.to_string();
            if let Some(h) = scan(name) {
                return Some(DiffHit {
                    path: String::new(),
                    line: None,
                    format: h.format,
                });
            }
            continue;
        }
        if l.starts_with("+++ ") || l.starts_with("--- ") {
            continue;
        }
        if let Some(h) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@: the first added line is c.
            line = h
                .split_whitespace()
                .find_map(|p| p.strip_prefix('+'))
                .and_then(|p| p.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            continue;
        }
        if let Some(added) = l.strip_prefix('+') {
            if let Some(h) = scan(added) {
                return Some(DiffHit {
                    path,
                    line: Some(line),
                    format: h.format,
                });
            }
            line += 1;
        }
    }
    None
}

/// `text` in a code span whose backticks no backtick run in it can close.
pub fn code_span(text: &str) -> String {
    let text = text.replace('\n', " ");
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let ticks = "`".repeat(longest + 1);
    // A space keeps a leading or trailing backtick from joining the fence.
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

/// `text` in a code block whose fence no line of it can close.
pub fn fenced(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{text}\n{fence}")
}

/// Fenced blocks GitHub renders as diagrams or maps rather than code.
const RENDERED_FENCES: &[&str] = &["mermaid", "geojson", "topojson", "stl"];

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
}

/// Agent Markdown as actuo posts it: images become `[image: <alt>] <url>` with
/// both in code spans; links (inline or by reference) become their text, then the
/// target in a code span, so no link hides where it goes; raw HTML goes in a code
/// span (inline) or a code block, where GitHub neither renders it nor links a URL
/// in it; diagram and map fences become plain code; `@` mentions go in code spans.
/// If a second look still finds any of those, the whole text goes in a code block
/// instead.
pub fn neutralize(md: &str) -> String {
    guard(md, apply(md, edits(md)))
}

/// `out` if nothing in it is live any more, else all of `md` in a code block.
fn guard(md: &str, out: String) -> String {
    if edits(&out).is_empty() {
        out
    } else {
        fenced(md)
    }
}

fn apply(md: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(md.len());
    let mut at = 0;
    for (r, text) in edits {
        if r.start < at {
            continue; // inside an edit already made
        }
        out.push_str(&md[at..r.start]);
        out.push_str(&text);
        at = r.end;
    }
    out.push_str(&md[at..]);
    out
}

/// A run of text outside code: its source range and what it reads as.
struct TextRun {
    range: Range<usize>,
    text: String,
}

/// A link being read: its source range, its target, where its text ends so far,
/// and whether it's left as it is (an autolink shows its target already).
struct OpenLink {
    range: Range<usize>,
    url: String,
    text_end: usize,
    keep: bool,
    /// `[ref][]`: the parser's range stops before the `[]`.
    collapsed: bool,
}

/// What has to change in `md`: images, links, HTML, diagram fences, mentions.
fn edits(md: &str) -> Vec<(Range<usize>, String)> {
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut image: Option<(Range<usize>, String, String, usize)> = None;
    let mut links: Vec<OpenLink> = Vec::new();
    let mut html_block = false;
    // The end of the last inline HTML edit, to merge one that follows it directly.
    let mut html_end = None::<usize>;
    let mut in_code = false;
    let mut run: Option<TextRun> = None;
    let flush = |run: &mut Option<TextRun>, edits: &mut Vec<(Range<usize>, String)>| {
        let Some(r) = run.take() else { return };
        let source = &md[r.range.clone()];
        if source == r.text {
            for m in mentions(md, r.range.clone()) {
                edits.push((m.clone(), code_span(&md[m])));
            }
        } else if !mentions(&r.text, 0..r.text.len()).is_empty() {
            // An escaped or entity-written `@`: GitHub still sees a mention.
            edits.push((r.range, code_span(&r.text)));
        }
    };
    for (event, range) in Parser::new_ext(md, options()).into_offset_iter() {
        // Everything inside a link is its text (images included).
        if !matches!(
            event,
            Event::Start(Tag::Link { .. }) | Event::End(TagEnd::Link)
        ) && let Some(link) = links.last_mut()
        {
            link.text_end = link.text_end.max(range.end);
        }
        if html_block {
            if let Event::End(TagEnd::HtmlBlock) = event {
                html_block = false;
            }
            continue;
        }
        if let Some((_, alt, _, depth)) = image.as_mut() {
            match &event {
                Event::Start(Tag::Image { .. }) => *depth += 1,
                Event::End(TagEnd::Image) if *depth > 0 => *depth -= 1,
                Event::End(TagEnd::Image) => {
                    let (r, alt, url, _) = image.take().expect("in an image");
                    let mut text = format!("[image: {}]", code_span(alt.trim()));
                    if !url.is_empty() {
                        text.push(' ');
                        text.push_str(&code_span(&url));
                    }
                    edits.push((r, text));
                }
                Event::Text(t) | Event::Code(t) => alt.push_str(t),
                _ => {}
            }
            continue;
        }
        if let Event::Text(text) = &event
            && !in_code
        {
            // Pieces next to each other (or with only an escaping backslash between)
            // read as one run.
            match run.as_mut() {
                Some(r)
                    if r.range.end == range.start
                        || (r.range.end + 1 == range.start
                            && &md[r.range.end..range.start] == "\\") =>
                {
                    r.range.end = range.end;
                    r.text.push_str(text);
                }
                _ => {
                    flush(&mut run, &mut edits);
                    run = Some(TextRun {
                        range: range.clone(),
                        text: text.to_string(),
                    });
                }
            }
            continue;
        }
        flush(&mut run, &mut edits);
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                image = Some((range, String::new(), dest_url.to_string(), 0));
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => links.push(OpenLink {
                text_end: range.start + 1,
                range,
                url: dest_url.to_string(),
                keep: matches!(link_type, LinkType::Autolink | LinkType::Email),
                collapsed: link_type == LinkType::Collapsed,
            }),
            Event::End(TagEnd::Link) => {
                if let Some(link) = links.pop()
                    && !link.keep
                {
                    // `[text](url)`, `[text][ref]`, `[ref]`: the `[` goes, and from
                    // the end of the text on becomes the target in a code span.
                    let mut r = link.range;
                    if link.collapsed && md[r.end..].starts_with("[]") {
                        r.end += 2;
                    }
                    edits.push((r.start..r.start + 1, String::new()));
                    edits.push((link.text_end..r.end, format!(" {}", code_span(&link.url))));
                }
            }
            Event::Start(Tag::HtmlBlock) => {
                html_block = true;
                let block = &md[range.clone()];
                let body = block.trim_end_matches('\n');
                let tail = &block[body.len()..];
                edits.push((range, format!("{}{tail}", fenced(body))));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                if let CodeBlockKind::Fenced(info) = kind {
                    let lang = info.split_whitespace().next().unwrap_or("");
                    if RENDERED_FENCES.contains(&lang.to_ascii_lowercase().as_str()) {
                        let block = &md[range.clone()];
                        let first = block.find('\n').unwrap_or(block.len());
                        let line = &block[..first];
                        let fence_end = line
                            .find(|c: char| c != ' ' && c != '`' && c != '~')
                            .unwrap_or(line.len());
                        edits.push((
                            range.start..range.start + first,
                            format!("{}text", &line[..fence_end]),
                        ));
                    }
                }
            }
            Event::End(TagEnd::CodeBlock) => in_code = false,
            Event::Html(_) | Event::InlineHtml(_) => {
                // `<a ...></a>`: one span, not two that touch.
                let start = match html_end {
                    Some(end) if end == range.start => {
                        edits.pop().expect("the edit before").0.start
                    }
                    _ => range.start,
                };
                html_end = Some(range.end);
                edits.push((start..range.end, code_span(&md[start..range.end])));
            }
            _ => {}
        }
    }
    flush(&mut run, &mut edits);
    edits
}

/// `@user` and `@org/team` mentions within `within` of `text`: an `@` not after a
/// letter, digit or one of `` _.@/-` `` (so not an email address), then a GitHub
/// login (a letter or digit first, up to 39 of letters, digits and `-`), then
/// optionally `/` and a team name.
fn mentions(text: &str, within: Range<usize>) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let login = |c: u8| c.is_ascii_alphanumeric() || c == b'-';
    let team = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.');
    let mut found = Vec::new();
    let mut i = within.start;
    while i < within.end {
        if b[i] != b'@' {
            i += 1;
            continue;
        }
        let after_word = i > 0
            && (b[i - 1].is_ascii_alphanumeric()
                || matches!(b[i - 1], b'_' | b'.' | b'@' | b'/' | b'-' | b'`'));
        let start = i;
        let mut j = i + 1;
        if after_word || j >= within.end || !b[j].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        while j < within.end && j - start <= 39 && login(b[j]) {
            j += 1;
        }
        if j + 1 < within.end && b[j] == b'/' && b[j + 1].is_ascii_alphanumeric() {
            j += 1;
            while j < within.end && team(b[j]) {
                j += 1;
            }
            while b[j - 1] == b'.' {
                j -= 1;
            }
        }
        found.push(start..j);
        i = j;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic value of `len` characters with plenty of distinct ones.
    fn random(len: usize) -> String {
        "aZ3kQ9xR2mT7vB4nL8pW6cY1"
            .chars()
            .cycle()
            .take(len)
            .collect()
    }

    /// The only test in this binary that keeps passed values (`keep` keeps the
    /// first call's).
    #[test]
    fn passed_values_are_held_back() {
        let value = "plain-looking passed value 4821";
        crate::pass_env::keep([("SOME_KEY".to_string(), value.to_string())].into());
        assert_eq!(
            scan(&format!("ok\nthe key is {value}, see")),
            Some(Hit {
                format: PASSED,
                line: 2
            })
        );
        // The format found first wins: here the Anthropic token comes before it.
        let token = format!("sk-ant-oat01-{}", random(95));
        assert_eq!(
            scan(&format!("{token} {value}")).map(|h| h.format),
            Some("an Anthropic key or token")
        );
        assert_eq!(
            scan_diff(&format!(
                "+++ b/notes.txt\n@@ -0,0 +1,2 @@\n+fine\n+{value}\n"
            )),
            Some(DiffHit {
                path: "notes.txt".into(),
                line: Some(2),
                format: PASSED
            })
        );
        assert_eq!(scan("plain-looking passed value"), None);
    }

    #[test]
    fn formats_match_real_shapes() {
        let cases = [
            (
                format!("sk-ant-oat01-{}", random(95)),
                "an Anthropic key or token",
            ),
            (
                format!("sk-ant-ort01-{}", random(95)),
                "an Anthropic key or token",
            ),
            (
                format!("sk-ant-api03-{}", random(95)),
                "an Anthropic key or token",
            ),
            (format!("sk-proj-{}", random(48)), "an OpenAI API key"),
            (
                format!("eyJ{}.eyJ{}.{}", random(30), random(60), random(43)),
                "a JWT",
            ),
            (format!("rt.1.{}", random(190)), "a Codex refresh token"),
            (format!("ghp_{}", random(36)), "a GitHub token"),
            (format!("ghs_{}", random(36)), "a GitHub token"),
            (format!("github_pat_{}", random(82)), "a GitHub token"),
            (format!("npm_{}", random(36)), "an npm token"),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----".into(), // gitleaks:allow (only the header line, to test the detector)
                "a private key",
            ),
            ("-----BEGIN RSA PRIVATE KEY-----".into(), "a private key"),
            (
                "-----BEGIN PGP PRIVATE KEY BLOCK-----".into(), // gitleaks:allow (only the header line, to test the detector)
                "a private key",
            ),
        ];
        for (value, format) in cases {
            let text = format!("line one\nthe key: {value} done\n");
            assert_eq!(scan(&text), Some(Hit { format, line: 2 }), "{value}");
        }
    }

    #[test]
    fn placeholders_and_prose_dont_count() {
        for text in [
            "Set ANTHROPIC_API_KEY=sk-ant-... in your shell.".to_string(),
            "a `ghp_` token, or `github_pat_…`".to_string(),
            format!("ghp_{}", "x".repeat(36)),
            format!("sk-ant-api03-{}", "A".repeat(95)),
            format!("npm_{}", &"0123456".repeat(6)[..36]),
            format!("ghp_{}", random(20)),
            // Inside a longer word, not a key.
            format!("desk-{}", random(60)),
            "-----BEGIN PUBLIC KEY-----".to_string(),
            "eyJhbGciOiJIUzI1NiJ9 alone".to_string(),
        ] {
            assert_eq!(scan(&text), None, "{text}");
        }
    }

    #[test]
    fn diff_hits_name_the_file_and_line() {
        let token = format!("ghp_{}", random(36));
        let diff = format!(
            "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -3,0 +4,2 @@\n+const a = 1;\n+const t = \"{token}\";\n"
        );
        assert_eq!(
            scan_diff(&diff),
            Some(DiffHit {
                path: "src/a.ts".into(),
                line: Some(5),
                format: "a GitHub token"
            })
        );
        // Removed lines don't count.
        let removed = format!("--- a/x\n+++ b/x\n@@ -1 +0,0 @@\n-{token}\n");
        assert_eq!(scan_diff(&removed), None);
        // A file name with one: the name isn't repeated.
        let named = format!(
            "diff --git a/x b/{token}\nnew file mode 100644\n--- /dev/null\n+++ b/{token}\n"
        );
        assert_eq!(
            scan_diff(&named),
            Some(DiffHit {
                path: String::new(),
                line: None,
                format: "a GitHub token"
            })
        );
    }

    #[test]
    fn spans_and_fences_outlast_backticks() {
        assert_eq!(code_span("a"), "`a`");
        assert_eq!(code_span("x``y"), "```x``y```");
        assert_eq!(code_span("`a"), "`` `a ``");
        assert_eq!(fenced("a"), "```\na\n```");
        assert_eq!(fenced("x ``` y"), "````\nx ``` y\n````");
    }

    #[test]
    fn images_become_text() {
        assert_eq!(
            neutralize("See ![chart](https://e.test/?d=1) here."),
            "See [image: `chart`] `https://e.test/?d=1` here."
        );
        assert_eq!(
            neutralize("![a][r]\n\n[r]: https://e.test/x"),
            "[image: `a`] `https://e.test/x`\n\n[r]: https://e.test/x"
        );
        // An image inside a link: both show their targets.
        assert_eq!(
            neutralize("[![b](https://e.test/i)](https://e.test/l)"),
            "[image: `b`] `https://e.test/i` `https://e.test/l`"
        );
        // A URL with backticks stays in its span.
        let out = neutralize("![x](https://e.test/a`b)");
        assert!(out.contains("``https://e.test/a`b``"), "{out}");
        for md in [
            "![x](https://e.test/?d=1)",
            "![![in](https://e.test/1)](https://e.test/2)",
        ] {
            assert!(edits(&neutralize(md)).is_empty(), "{md}");
        }
    }

    /// Every link shows where it goes: its text, then its target in a code span,
    /// which GitHub doesn't make into a link.
    #[test]
    fn links_show_their_targets() {
        assert_eq!(
            neutralize("See [docs](https://evil.test/d) now."),
            "See docs `https://evil.test/d` now."
        );
        assert_eq!(
            neutralize("A [**bold** `code` link](https://e.test \"title\")."),
            "A **bold** `code` link `https://e.test`."
        );
        // By reference: full, collapsed and shortcut. The definitions render as
        // nothing on GitHub and stay.
        assert_eq!(
            neutralize("[text][r], [r][] and [r].\n\n[r]: https://evil.test/r"),
            "text `https://evil.test/r`, r `https://evil.test/r` and r `https://evil.test/r`.\n\n[r]: https://evil.test/r"
        );
        // A target with backticks stays inside its span.
        let out = neutralize("[x](https://e.test/a``b`c)");
        assert_eq!(out, "x ```https://e.test/a``b`c```");
        let out = neutralize("[x](<https://e.test/`a b`>)");
        assert_eq!(out, "x `` https://e.test/`a b` ``");
        // A link's text that looks like a URL: GitHub links that text to itself,
        // and the real target is shown next to it.
        assert_eq!(
            neutralize("[https://good.test](https://evil.test)"),
            "https://good.test `https://evil.test`"
        );
        assert_eq!(neutralize("[](https://e.test)"), " `https://e.test`");
        // An autolink already shows its target.
        assert_eq!(neutralize("<https://e.test/x>"), "<https://e.test/x>");
        for md in [
            "[docs](https://evil.test/d)",
            "[t][r]\n\n[r]: https://e.test",
            "[x](https://e.test/a``b`c)",
            "[a <b>c</b>](https://e.test)",
        ] {
            assert!(edits(&neutralize(md)).is_empty(), "{md}");
        }
        // A rewrite that makes a new link fails closed: the whole text is code.
        let md = "[[a](https://e.test/1)](https://e.test/2)";
        assert_eq!(neutralize(md), format!("```\n{md}\n```"));
    }

    /// Raw HTML goes in a code span (inline) or block: not rendered, and no URL in
    /// it linked.
    #[test]
    fn html_goes_in_code() {
        assert_eq!(
            neutralize("a <img src=\"https://e.test/?d=1\"> b"),
            "a `<img src=\"https://e.test/?d=1\">` b"
        );
        assert_eq!(
            neutralize("go <a href=\"https://evil.test\">here</a>."),
            "go `<a href=\"https://evil.test\">`here`</a>`."
        );
        assert_eq!(neutralize("x <!-- hidden --> y"), "x `<!-- hidden -->` y");
        let block = "<picture>\n<source srcset=\"https://e.test/x\">\n</picture>\n\nafter";
        assert_eq!(
            neutralize(block),
            "```\n<picture>\n<source srcset=\"https://e.test/x\">\n</picture>\n```\n\nafter"
        );
        let a = "<a href=\"https://evil.test\">\nclick\n</a>\n";
        assert_eq!(
            neutralize(a),
            "```\n<a href=\"https://evil.test\">\nclick\n</a>\n```\n"
        );
        // Tags next to each other: one span.
        let video = neutralize("<video src=\"https://e.test/v\"></video>");
        assert_eq!(video, "`<video src=\"https://e.test/v\"></video>`");
        // Backticks in the HTML can't close its span or block.
        let out = neutralize("a <b title=\"``\">x</b>");
        assert_eq!(out, "a ```<b title=\"``\">```x`</b>`");
        for md in [block, a, "go <a href=\"https://evil.test\">here</a>."] {
            assert!(edits(&neutralize(md)).is_empty(), "{md}");
        }
    }

    #[test]
    fn code_is_left_alone() {
        for md in [
            "Use `![x](u)` and `<img>` and `@someone`.",
            "```\n![x](https://e.test/1)\n<img src=x>\n@someone\n```",
            "    ![x](https://e.test/1) indented code",
        ] {
            assert_eq!(neutralize(md), md);
        }
    }

    #[test]
    fn diagram_fences_become_code() {
        assert_eq!(
            neutralize("```mermaid\ngraph TD; A-->B\n```"),
            "```text\ngraph TD; A-->B\n```"
        );
        assert_eq!(neutralize("~~~ geojson\n{}\n~~~"), "~~~ text\n{}\n~~~");
        assert_eq!(neutralize("```ts\nlet a\n```"), "```ts\nlet a\n```");
    }

    #[test]
    fn mentions_go_in_code_spans() {
        assert_eq!(
            neutralize("Thanks @someone and @org/team-a."),
            "Thanks `@someone` and `@org/team-a`."
        );
        assert_eq!(
            neutralize("Mail a@example.com or x.y@mail.example.org"),
            "Mail a@example.com or x.y@mail.example.org"
        );
        assert_eq!(neutralize("cc **@lead**"), "cc **`@lead`**");
        // Written so GitHub still sees a mention: the whole text run goes in a span.
        assert_eq!(neutralize("hi \\@someone"), "`hi @someone`");
        assert_eq!(neutralize("hi &#64;someone"), "`hi @someone`");
        assert!(edits(&neutralize("[@x](https://e.test) and @y")).is_empty());
    }

    #[test]
    fn fails_closed() {
        // A rewrite that left something live: the whole text goes in a code block.
        let md = "a ![x](https://e.test/1) @someone";
        assert_eq!(guard(md, md.to_string()), format!("```\n{md}\n```"));
        assert!(edits(&guard(md, md.to_string())).is_empty());
    }

    #[test]
    fn actuo_text_is_unchanged() {
        for text in [
            "**Actuo:** waiting (request)\n\n- Agent: `claude`, level `edits`, model `the agent's default`\n- Session: `s1` (`actuo attach s1`)\n- Setup `pnpm install --frozen-lockfile`: passed\n- Run: `r1`",
            "The agent asks to run a command:\n\n```\npnpm test\n```\nNo rule covers it, so the run waits for you: answer it with `actuo attach s1`.",
            "Draft PR: https://github.com/o/r/pull/2",
            "The run failed (setup_failed).\n\nSetup `pnpm i` failed (exit 1):\n\n```\nERR <x>\n```\n\nSession: `s1`.",
        ] {
            assert_eq!(neutralize(text), text);
        }
    }
}
