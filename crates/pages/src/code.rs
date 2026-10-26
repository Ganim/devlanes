/// Why the proxy answered a request itself. Each code has a stable slug ([`Self::as_str`],
/// the docs page and the `x-devlanes-error` header) and a stable ID ([`Self::id`]).
///
/// The discriminant is the ID's number: 1xx routing and request errors, 2xx service
/// state, 3xx upstream errors. Never renumber or reuse one; add new codes at the end of
/// their group.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u16)]
pub enum ErrorCode {
    /// `DL101`: no route matches the requested hostname.
    UnknownHost = 101,
    /// `DL102`: the request had no usable Host or `:authority`.
    InvalidHost = 102,
    /// `DL103`: an HTTP CONNECT tunnel was requested.
    ConnectUnsupported = 103,
    /// `DL104`: the request already passed through this route once.
    ProxyLoop = 104,
    /// `DL201`: the route's service is not running.
    ServiceIdle = 201,
    /// `DL202`: the route's service did not become ready while the proxy waited.
    ServiceStarting = 202,
    /// `DL203`: the route's service exited or failed its readiness check.
    ServiceFailed = 203,
    /// `DL204`: the route's box stayed disconnected while the proxy waited.
    BoxReconnecting = 204,
    /// `DL301`: nothing accepted the connection on the service's port, even after retries.
    UpstreamUnreachable = 301,
    /// `DL302`: the service closed the connection or sent an invalid response.
    UpstreamFailed = 302,
    /// `DL303`: the service switched protocols on an ordinary request.
    UnexpectedUpgrade = 303,
}

/// One troubleshooting step; `{unit}` in a command becomes the route's unit reference.
pub(crate) struct Check {
    pub text: &'static str,
    pub command: Option<&'static str>,
}

/// Shorthand for a step with a command.
const fn run(text: &'static str, command: &'static str) -> Check {
    Check {
        text,
        command: Some(command),
    }
}

/// Shorthand for a step without a command.
const fn note(text: &'static str) -> Check {
    Check {
        text,
        command: None,
    }
}

const LOGS: Check = run("Read its output", "devlanes logs {unit} --tail 100");

/// How bad an error is; pages color their title and ID by it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    /// Nothing is broken; the user has to act, such as starting a stopped service.
    Info,
    /// Expected to clear up on its own.
    Warning,
    /// Something is broken or misconfigured.
    Error,
}

impl Severity {
    /// CSS class suffix and Markdown label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl ErrorCode {
    /// Every code, by ID; the docs list them in this order.
    pub const ALL: [Self; 11] = [
        Self::UnknownHost,
        Self::InvalidHost,
        Self::ConnectUnsupported,
        Self::ProxyLoop,
        Self::ServiceIdle,
        Self::ServiceStarting,
        Self::ServiceFailed,
        Self::BoxReconnecting,
        Self::UpstreamUnreachable,
        Self::UpstreamFailed,
        Self::UnexpectedUpgrade,
    ];

    /// The stable ID, such as `DL101`.
    pub fn id(self) -> String {
        format!("DL{}", self as u16)
    }

    /// Looks a code up by its ID (`DL101`, case-insensitive) or slug (`unknown-host`).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        Self::ALL
            .into_iter()
            .find(|code| text.eq_ignore_ascii_case(&code.id()) || text == code.as_str())
    }

    /// The stable slug, such as `unknown-host`; also the docs page's file name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidHost => "invalid-host",
            Self::UnknownHost => "unknown-host",
            Self::ConnectUnsupported => "connect-unsupported",
            Self::ProxyLoop => "proxy-loop",
            Self::ServiceIdle => "service-idle",
            Self::ServiceStarting => "service-starting",
            Self::ServiceFailed => "service-failed",
            Self::BoxReconnecting => "box-reconnecting",
            Self::UpstreamUnreachable => "upstream-unreachable",
            Self::UpstreamFailed => "upstream-failed",
            Self::UnexpectedUpgrade => "unexpected-upgrade",
        }
    }

    /// HTTP status the proxy responds with.
    pub fn status(self) -> u16 {
        match self {
            Self::InvalidHost => 400,
            Self::UnknownHost => 404,
            Self::ConnectUnsupported => 405,
            Self::ProxyLoop => 508,
            Self::ServiceIdle
            | Self::ServiceStarting
            | Self::ServiceFailed
            | Self::BoxReconnecting
            | Self::UpstreamUnreachable => 503,
            Self::UpstreamFailed | Self::UnexpectedUpgrade => 502,
        }
    }

    /// How bad the error is.
    pub fn severity(self) -> Severity {
        match self {
            Self::ServiceIdle => Severity::Info,
            Self::ServiceStarting | Self::BoxReconnecting => Severity::Warning,
            _ => Severity::Error,
        }
    }

    /// Reason phrase of [`Self::status`].
    pub(crate) fn reason(self) -> &'static str {
        match self.status() {
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "Loop Detected",
        }
    }

    /// Page heading.
    pub fn title(self) -> &'static str {
        match self {
            Self::InvalidHost => "Invalid host",
            Self::UnknownHost => "Unknown host",
            Self::ConnectUnsupported => "CONNECT not supported",
            Self::ProxyLoop => "Proxy loop",
            Self::ServiceIdle => "Service not running",
            Self::ServiceStarting => "Service starting",
            Self::ServiceFailed => "Service failed",
            Self::BoxReconnecting => "Box reconnecting",
            Self::UpstreamUnreachable => "Service not listening",
            Self::UpstreamFailed => "Upstream request failed",
            Self::UnexpectedUpgrade => "Unexpected protocol upgrade",
        }
    }

    /// The facts behind the error that its title does not already say.
    pub fn summary(self) -> &'static str {
        match self {
            Self::InvalidHost => {
                "The request has no Host header, several, or one that is invalid or disagrees with the HTTP/2 :authority."
            }
            Self::UnknownHost => {
                "No lane routes this hostname. While the daemon runs, it answers every request to this machine's HTTP and HTTPS ports, including requests meant for other servers."
            }
            Self::ConnectUnsupported => {
                "devlanes does not open CONNECT tunnels. WebSockets work over an HTTP/1.1 Upgrade."
            }
            Self::ProxyLoop => {
                "This request already passed through this route. A dev server proxy probably sent it back to devlanes with the original Host header."
            }
            Self::ServiceIdle => "The service behind this hostname is stopped.",
            Self::ServiceStarting => {
                "The proxy held the request while the service started, but its ready check did not pass in time. Reload when it is ready."
            }
            Self::ServiceFailed => "The service exited or its ready check failed.",
            Self::BoxReconnecting => {
                "The service runs on a remote box and the connection to the box is down. The proxy held the request, but the reconnect is taking longer; reload in a moment."
            }
            Self::UpstreamUnreachable => {
                "devlanes has the service as running, but nothing accepted connections on its port while the proxy retried."
            }
            Self::UpstreamFailed => {
                "The service closed the connection or sent an invalid HTTP/1.1 response."
            }
            Self::UnexpectedUpgrade => {
                "The service answered 101 Switching Protocols to a request that did not ask for an upgrade."
            }
        }
    }

    /// Steps to diagnose this error, most useful first.
    pub(crate) fn checks(self) -> Vec<Check> {
        match self {
            Self::UnknownHost => vec![
                run("List routed hostnames", "devlanes url ls"),
                run("Validate `devlanes.yaml`", "devlanes config check"),
                run(
                    "For a lane on a box, check the connection",
                    "devlanes remote ls",
                ),
                run(
                    "Free the ports for another server; this stops every service",
                    "devlanes daemon stop",
                ),
                run("Check the proxy, CA and hosts file", "devlanes doctor"),
            ],
            Self::ServiceIdle => vec![
                run("Start the service", "devlanes start {unit}"),
                run("Show why it is not running", "devlanes debug why {unit}"),
            ],
            Self::ServiceStarting => vec![
                run("Wait for it", "devlanes wait {unit}"),
                run("Show what it waits for", "devlanes debug why {unit}"),
                LOGS,
            ],
            Self::ServiceFailed => vec![
                run("Show the cause and a fix", "devlanes debug why {unit}"),
                run(
                    "Read its output since the last start",
                    "devlanes logs {unit} --since start",
                ),
                run("Restart it", "devlanes restart {unit}"),
            ],
            Self::BoxReconnecting => vec![
                run("Show the connection state", "devlanes remote ls"),
                run(
                    "Reconnect in a terminal that can answer SSH prompts",
                    "devlanes remote connect <box>",
                ),
            ],
            Self::UpstreamUnreachable => vec![
                note("Make the service listen on `PORT`, on 127.0.0.1 or ::1"),
                run("Compare with the routed port", "devlanes url ls"),
                run("Show what devlanes sees", "devlanes debug why {unit}"),
            ],
            Self::UpstreamFailed => vec![
                note(
                    "Serve plain HTTP/1.1 on the service's port; devlanes terminates TLS, so HTTPS or HTTP/2-only services cannot answer it",
                ),
                LOGS,
            ],
            Self::UnexpectedUpgrade => vec![LOGS],
            Self::ProxyLoop => vec![
                note(
                    "Point the dev server proxy (Vite `server.proxy`, Next.js rewrites) at the target service's port, not at devlanes",
                ),
                note("Or make it rewrite Host to the target (Vite `changeOrigin: true`)"),
                run("Find the target's port", "devlanes url ls"),
            ],
            Self::InvalidHost => vec![note(
                "Send one Host header that matches the URL, as `curl http://acme.localhost/` does",
            )],
            Self::ConnectUnsupported => vec![note(
                "Unset `HTTP_PROXY` and `HTTPS_PROXY` if they point at devlanes",
            )],
        }
    }
}

impl std::fmt::Display for ErrorCode {
    /// Writes the stable slug.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
