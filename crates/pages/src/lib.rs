//! Error pages the devlanes proxy serves when it answers a request itself instead of
//! forwarding it: unknown hosts, stopped or failed services, upstream errors.
//!
//! Pages are self-contained (inline CSS and logo, no external requests) because they are
//! served offline. Each one carries a stable [`ErrorCode`], troubleshooting commands and
//! links to the docs and the issue tracker. Clients that do not ask for HTML get the same
//! content as Markdown.

mod code;

use askama::Template;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

pub use code::{ErrorCode, Severity};

/// The page stylesheet, compiled from `styles/page.scss` by `build.rs`.
const STYLE: &str = include_str!(concat!(env!("OUT_DIR"), "/page.css"));

/// Docs index of every [`ErrorCode`]; each code has its own page below it, published
/// from `docs/errors/<slug>.md`.
pub const DOCS_URL: &str = "https://devlanes.run/docs/errors/";

/// Allows the inline stylesheet by hash and nothing else: no scripts, images or fetches.
pub const CONTENT_SECURITY_POLICY: &str = concat!(
    "default-src 'none'; style-src 'sha256-",
    env!("DEVLANES_PAGES_STYLE_SHA256"),
    "'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
);

/// Known hostnames listed on an unknown-host page, at most this many.
const MAX_KNOWN_HOSTS: usize = 50;

/// Response body formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Html,
    Markdown,
}

impl Format {
    /// Picks HTML when `Accept` asks for it at least as much as for Markdown or plain
    /// text, and Markdown otherwise. Browsers send `text/html`; curl, agents and
    /// `fetch()` send `*/*` or nothing and get Markdown.
    pub fn negotiate(accept: Option<&str>) -> Self {
        let (mut html, mut markdown) = (0.0_f32, 0.0_f32);
        for range in accept.unwrap_or_default().split(',') {
            let mut parts = range.split(';');
            let media = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
            let quality = parts
                .filter_map(|param| param.trim().strip_prefix("q="))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            match media.as_str() {
                "text/html" | "application/xhtml+xml" => html = html.max(quality),
                "text/markdown" | "text/plain" => markdown = markdown.max(quality),
                _ => {}
            }
        }
        if html > 0.0 && html >= markdown {
            Self::Html
        } else {
            Self::Markdown
        }
    }

    /// `Content-Type` header value.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Html => "text/html; charset=utf-8",
            Self::Markdown => "text/markdown; charset=utf-8",
        }
    }
}

/// A routed hostname suggested on an unknown-host page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnownHost<'a> {
    pub host: &'a str,
    /// Project and lane, such as `acme@2`.
    pub owner: Option<&'a str>,
}

/// One error response. Everything except the code is optional.
#[derive(Clone, Debug)]
pub struct ErrorPage<'a> {
    code: ErrorCode,
    host: &'a str,
    hint: &'a str,
    unit: Option<String>,
    logs: &'a str,
    known_hosts: Vec<KnownHost<'a>>,
    remote: bool,
}

impl<'a> ErrorPage<'a> {
    /// A page with the code's title, summary and checks only.
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            host: "",
            hint: "",
            unit: None,
            logs: "",
            known_hosts: Vec::new(),
            remote: false,
        }
    }

    /// The route belongs to a lane on a remote box, where its commands have to run.
    pub fn remote(mut self, remote: bool) -> Self {
        self.remote = remote;
        self
    }

    /// The hostname the client requested.
    pub fn host(mut self, host: &'a str) -> Self {
        self.host = host;
        self
    }

    /// An extra line from the daemon, such as the command that starts the lane.
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = hint;
        self
    }

    /// The unit behind the route, as `project@lane` and unit name; troubleshooting
    /// commands then name it instead of `<service>`.
    pub fn unit(mut self, lane: Option<&str>, unit: Option<&str>) -> Self {
        self.unit = match (lane, unit) {
            (Some(lane), Some(unit)) => Some(format!("{lane}:{unit}")),
            (None, Some(unit)) => Some(unit.to_string()),
            _ => None,
        };
        self
    }

    /// The service's recent output. Callers decide who may see it.
    pub fn logs(mut self, logs: &'a str) -> Self {
        self.logs = logs;
        self
    }

    /// Routed hostnames to list and suggest from. Callers decide who may see them.
    pub fn known_hosts(mut self, hosts: Vec<KnownHost<'a>>) -> Self {
        self.known_hosts = hosts;
        self
    }

    /// The error code.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Renders the page; dynamic values are escaped for the format.
    pub fn render(&self, format: Format) -> String {
        let view = self.view();
        let rendered = match format {
            Format::Html => HtmlPage {
                v: &view,
                style: STYLE,
            }
            .render(),
            Format::Markdown => MarkdownPage {
                v: &view,
                fence: fence(self.logs),
            }
            .render(),
        };
        // Rendering only fails when a Display impl does, which plain strings never do.
        rendered
            .unwrap_or_else(|_| format!("devlanes proxy error {} {}\n", self.code.id(), self.code))
    }

    /// Resolves everything the templates show.
    fn view(&self) -> View<'_> {
        let unit = self.unit.as_deref().unwrap_or("<service>");
        let mut checks: Vec<ViewCheck> = self
            .code
            .checks()
            .into_iter()
            .map(|check| ViewCheck {
                text: check.text,
                command: check.command.map(|command| command.replace("{unit}", unit)),
            })
            .collect();
        // The daemon's hint is a bare start command for local lanes, which only adds to
        // the checks of a stopped service, and a sentence naming the box for box lanes.
        let mut hint = self.hint;
        if hint.starts_with("devlanes ") {
            if self.code == ErrorCode::ServiceIdle {
                checks.insert(
                    0,
                    ViewCheck {
                        text: "Start the whole lane",
                        command: Some(hint.to_string()),
                    },
                );
            }
            hint = "";
        }
        let mut known: Vec<KnownHost<'_>> = self.known_hosts.clone();
        known.sort_by(|a, b| a.host.cmp(b.host));
        known.dedup_by(|a, b| a.host == b.host);
        let suggestion = closest(self.host, &known);
        let hidden = known.len().saturating_sub(MAX_KNOWN_HOSTS);
        known.truncate(MAX_KNOWN_HOSTS);
        View {
            id: self.code.id(),
            code: self.code.as_str(),
            status: self.code.status(),
            reason: self.code.reason(),
            title: self.code.title(),
            summary: self.code.summary(),
            hint,
            severity: self.code.severity().as_str(),
            // The connection lives on this machine, so its problems are fixed here.
            remote: self.remote
                && !matches!(
                    self.code,
                    ErrorCode::BoxReconnecting | ErrorCode::BoxSignInNeeded
                ),
            logs: self.logs.trim_end(),
            suggestion,
            known,
            hidden,
            checks,
            docs_url: docs_url(self.code),
            issue_url: issue_url(self.code),
            version: devlanes_version::VERSION,
        }
    }
}

/// Docs page for `code`, such as `https://devlanes.run/docs/errors/unknown-host/`.
pub fn docs_url(code: ErrorCode) -> String {
    format!("{DOCS_URL}{code}/")
}

/// A new-issue link prefilled with only the error code and version: hostnames and logs
/// can be private, so the reporter adds those deliberately.
pub fn issue_url(code: ErrorCode) -> String {
    let (id, version) = (code.id(), devlanes_version::VERSION);
    let title = format!("Proxy error {id} {code}");
    let body = format!(
        "Error: {id} {code}\ndevlanes version: {version}\n\nWhat I expected and what happened instead:\n"
    );
    format!(
        "{}/issues/new?title={}&body={}",
        devlanes_version::REPOSITORY,
        utf8_percent_encode(&title, QUERY),
        utf8_percent_encode(&body, QUERY)
    )
}

/// RFC 3986 unreserved characters stay readable in the issue link.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

struct ViewCheck {
    text: &'static str,
    command: Option<String>,
}

impl ViewCheck {
    /// Splits the text on backticks: odd segments are inline code.
    fn segments(&self) -> Vec<(bool, &'static str)> {
        self.text
            .split('`')
            .enumerate()
            .map(|(index, part)| (index % 2 == 1, part))
            .collect()
    }
}

struct View<'a> {
    id: String,
    code: &'static str,
    status: u16,
    reason: &'static str,
    title: &'static str,
    summary: &'static str,
    hint: &'a str,
    severity: &'static str,
    remote: bool,
    logs: &'a str,
    suggestion: Option<KnownHost<'a>>,
    known: Vec<KnownHost<'a>>,
    hidden: usize,
    checks: Vec<ViewCheck>,
    docs_url: String,
    issue_url: String,
    version: &'static str,
}

// `HtmlPage`, derived from `templates/page.html` after `build.rs` minified it.
include!(concat!(env!("OUT_DIR"), "/html_page.rs"));

#[derive(Template)]
#[template(path = "page.md", escape = "none")]
struct MarkdownPage<'a> {
    v: &'a View<'a>,
    fence: String,
}

/// A code fence longer than any backtick run in `text`, so logs cannot close it early.
fn fence(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    "`".repeat(longest.max(2) + 1)
}

/// The known host closest to `host` by edit distance, when it is close enough to be a
/// likely typo or wrong lane number. Wildcards compare by their suffix.
fn closest<'a>(host: &str, known: &[KnownHost<'a>]) -> Option<KnownHost<'a>> {
    if host.is_empty() {
        return None;
    }
    known
        .iter()
        .map(|candidate| {
            let name = candidate.host.strip_prefix("*.").unwrap_or(candidate.host);
            (distance(host, name), *candidate)
        })
        .filter(|(distance, candidate)| {
            *distance > 0 && *distance <= (host.len().max(candidate.host.len()) / 3).max(2)
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance over bytes; hostnames are ASCII.
fn distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, &left) in a.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, &right) in b.iter().enumerate() {
            let substitution = previous + usize::from(left != right);
            previous = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(previous + 1);
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Browsers get HTML; curl, fetch and Markdown-preferring agents get Markdown.
    #[test]
    fn negotiates_format() {
        let browser = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
        assert_eq!(Format::negotiate(Some(browser)), Format::Html);
        assert_eq!(Format::negotiate(Some("*/*")), Format::Markdown);
        assert_eq!(Format::negotiate(None), Format::Markdown);
        assert_eq!(
            Format::negotiate(Some("text/markdown, text/html;q=0.5")),
            Format::Markdown
        );
        assert_eq!(Format::negotiate(Some("text/html;q=0")), Format::Markdown);
        assert_eq!(
            Format::negotiate(Some("application/json")),
            Format::Markdown
        );
    }

    /// Hosts, hints and logs are escaped in HTML and fenced in Markdown.
    #[test]
    fn escapes_dynamic_values() {
        let page = ErrorPage::new(ErrorCode::ServiceFailed)
            .hint("<b>hint</b>")
            .logs("<script>bad()</script>\n```\n");
        let html = page.render(Format::Html);
        assert!(html.contains("&#60;script&#62;") || html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<b>hint"));
        let markdown = page.render(Format::Markdown);
        assert!(markdown.contains("````\n<script>bad()</script>\n```\n````"));
    }

    /// The issue link carries the code and version, never the host or logs.
    #[test]
    fn issue_link_has_no_private_data() {
        let page = ErrorPage::new(ErrorCode::UnknownHost)
            .host("secret.acme.localhost")
            .logs("token=abc");
        let html = page.render(Format::Html);
        let url = issue_url(ErrorCode::UnknownHost);
        assert!(url.contains("title=Proxy%20error%20DL101%20unknown-host&"));
        assert!(url.contains(&format!("version%3A%20{}", devlanes_version::VERSION)));
        assert!(!url.contains("secret") && !url.contains("token"));
        assert!(
            html.contains(&url.replace('&', "&#38;")) || html.contains(&url.replace('&', "&amp;"))
        );
    }

    /// Commands name the route's unit when it is known.
    #[test]
    fn commands_name_the_unit() {
        let page = ErrorPage::new(ErrorCode::ServiceIdle).unit(Some("acme@2"), Some("api"));
        let markdown = page.render(Format::Markdown);
        assert!(markdown.contains("devlanes start acme@2:api"));
        let generic = ErrorPage::new(ErrorCode::ServiceIdle).render(Format::Markdown);
        assert!(generic.contains("devlanes start <service>"));
    }

    /// A box that needs a sign-in is fixed on this machine: its page names the
    /// box through the daemon's hint and does not send the reader to the box.
    #[test]
    fn sign_in_pages_are_diagnosed_here() {
        let page = ErrorPage::new(ErrorCode::BoxSignInNeeded)
            .unit(Some("acme@2"), Some("web"))
            .remote(true)
            .hint("Sign in to my-box again: devlanes remote connect my-box");
        let markdown = page.render(Format::Markdown);
        assert!(
            markdown.contains("Sign in to my-box again: devlanes remote connect my-box"),
            "{markdown}"
        );
        assert!(!markdown.contains("Run these on the box"), "{markdown}");
        assert!(
            markdown.contains("`devlanes remote connect <box>`"),
            "{markdown}"
        );
        let idle = ErrorPage::new(ErrorCode::ServiceIdle).remote(true);
        assert!(
            idle.render(Format::Markdown)
                .contains("Run these on the box")
        );
    }

    /// A wrong lane number or a typo suggests the routed host; unrelated hosts do not.
    #[test]
    fn suggests_close_hosts() {
        let known = [
            KnownHost {
                host: "acme-1.localhost",
                owner: Some("acme@1"),
            },
            KnownHost {
                host: "*.shop.localhost",
                owner: None,
            },
        ];
        let hit = |host| closest(host, &known).map(|known| known.host);
        assert_eq!(hit("acme-3.localhost"), Some("acme-1.localhost"));
        assert_eq!(hit("shpo.localhost"), Some("*.shop.localhost"));
        assert_eq!(hit("unrelated.example.invalid"), None);
    }

    /// Every code has a distinct slug and ID, parses back from both, and both formats
    /// show them.
    #[test]
    fn every_code_renders() {
        let (mut slugs, mut ids) = (HashSet::new(), HashSet::new());
        for code in ErrorCode::ALL {
            assert!(slugs.insert(code.as_str()));
            assert!(ids.insert(code.id()));
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
            assert_eq!(ErrorCode::parse(&code.id().to_lowercase()), Some(code));
            let page = ErrorPage::new(code);
            for format in [Format::Html, Format::Markdown] {
                let text = page.render(format);
                assert!(text.contains(code.title()));
                assert!(text.contains(&code.id()) && text.contains(code.as_str()));
            }
        }
        assert_eq!(ErrorCode::UnknownHost.id(), "DL101");
        assert_eq!(ErrorCode::parse("DL999"), None);
    }

    /// Every code has a docs page whose title and sidebar position match the enum, and
    /// the index lists it, so the links on the pages never point at a missing page.
    #[test]
    fn every_code_has_a_docs_page() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/errors");
        let index = std::fs::read_to_string(dir.join("README.md")).expect("docs/errors/README.md");
        for code in ErrorCode::ALL {
            let path = dir.join(format!("{code}.md"));
            let page = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let title = format!("\n# {} {}\n", code.id(), code.title());
            assert!(
                page.contains(&title),
                "{} needs the heading {title:?}",
                path.display()
            );
            let order = format!("sidebar:\n  order: {}\n", code as u16);
            assert!(
                page.starts_with(&format!("---\n{order}---\n")),
                "{} needs {order:?}",
                path.display()
            );
            assert!(
                index.contains(&format!("[{}]({code}.md)", code.id())),
                "index misses {code}"
            );
        }
    }

    /// IDs are listed in order, so `ALL` and the docs stay sorted.
    #[test]
    fn codes_are_sorted_by_id() {
        let numbers: Vec<u16> = ErrorCode::ALL.iter().map(|code| *code as u16).collect();
        assert!(numbers.is_sorted());
    }

    /// The HTML has no template line breaks or indentation; logs keep theirs.
    #[test]
    fn html_is_minified() {
        let page = ErrorPage::new(ErrorCode::ServiceFailed).unit(Some("acme@2"), Some("web"));
        let html = page.render(Format::Html);
        assert!(!html.contains('\n'), "{html}");
        assert!(html.contains("Sent by the devlanes"));
        let html = page.logs("line 1\n    line 2").render(Format::Html);
        assert!(html.contains("<pre>line 1\n    line 2</pre>"));
    }

    /// The page carries the severity class its stylesheet colors.
    #[test]
    fn severity_class() {
        let class = |code: ErrorCode| {
            let html = ErrorPage::new(code).render(Format::Html);
            ["info", "warning", "error"]
                .into_iter()
                .find(|severity| html.contains(&format!("class=\"severity-{severity}\"")))
        };
        assert_eq!(class(ErrorCode::ServiceIdle), Some("info"));
        assert_eq!(class(ErrorCode::ServiceStarting), Some("warning"));
        assert_eq!(class(ErrorCode::UpstreamFailed), Some("error"));
        assert!(STYLE.contains(".severity-error"));
    }

    /// The CSP hash matches the inlined stylesheet byte for byte.
    #[test]
    fn style_hash_matches() {
        let html = ErrorPage::new(ErrorCode::UnknownHost).render(Format::Html);
        assert!(html.contains(&format!("<style>{STYLE}</style>")));
    }
}
