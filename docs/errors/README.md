# Proxy errors

While the daemon runs, its proxy answers every request to this machine's HTTP and HTTPS ports: 80 and 443, or 8080 and 8443 when those were taken. When it does not forward a request to a service, it answers with an error page that names one of these errors.

| ID | Error | Status | Meaning |
| --- | --- | --- | --- |
| [DL101](unknown-host.md) | `unknown-host` | 404 | No lane routes the hostname. |
| [DL102](invalid-host.md) | `invalid-host` | 400 | The Host header is missing or invalid. |
| [DL103](connect-unsupported.md) | `connect-unsupported` | 405 | A client asked for a CONNECT tunnel. |
| [DL104](proxy-loop.md) | `proxy-loop` | 508 | A request came back to the route it left. |
| [DL201](service-idle.md) | `service-idle` | 503 | The service is stopped. |
| [DL202](service-starting.md) | `service-starting` | 503 | The service was not ready within 30 seconds. |
| [DL203](service-failed.md) | `service-failed` | 503 | The service exited or its ready check failed. |
| [DL204](box-reconnecting.md) | `box-reconnecting` | 503 | The service's box stayed disconnected for 15 seconds. |
| [DL205](box-sign-in-needed.md) | `box-sign-in-needed` | 503 | The service's box needs you to sign in again before it reconnects. |
| [DL301](upstream-unreachable.md) | `upstream-unreachable` | 503 | Nothing listened on the service's port for 5 seconds. |
| [DL302](upstream-failed.md) | `upstream-failed` | 502 | The service broke off the request. |
| [DL303](unexpected-upgrade.md) | `unexpected-upgrade` | 502 | The service switched protocols unasked. |

DL1xx errors concern the request, DL2xx the service's state and DL3xx the connection to the service. IDs and names keep their meaning across releases.

The proxy avoids an error page when it can do so without sending a request twice. Plain HTTP to a hostname served over HTTPS gets a redirect. A request to a starting service, or to a box that is reconnecting, waits until the service is reachable. A refused connection is retried while a restarting server comes back.

## Error responses

Error responses carry `x-devlanes-error` with the error name and are not cached. Browsers get HTML; clients that do not ask for `text/html`, such as curl and `fetch()`, get the same content as Markdown. The "Open an issue" link fills in only the error ID and the devlanes version.

The service's recent output on `service-failed` pages and the routed hostnames on `unknown-host` pages are shown only to clients on this machine, not to other devices when `proxy.lan` is on. The hostnames also need a request for `localhost`, an IP address or a `.localhost` name.

Responses forwarded from a service carry `Via: 1.1 devlanes`; `proxy.via: false` in the user config turns the header off.
