---
sidebar:
  order: 101
---

# DL101 Unknown host

`404`. No lane routes the requested hostname. While the daemon runs it owns this machine's HTTP and HTTPS ports, so requests meant for another server end up here too.

Over HTTPS the browser shows a certificate error instead of this page, because the proxy only has certificates for routed hostnames. Open the `http://` URL to see the page.

## Causes

- A typo or the wrong lane number. Lane 2 of `acme` is `acme-2.localhost`; its `api` service is `api.acme-2.localhost`.
- The daemon does not know the lane yet. Run `devlanes start` in the project or worktree.
- No `hosts` entry in `devlanes.yaml` declares the name.
- Another lane claimed the name first; devlanes warns about the conflict.
- The lane runs on a box that is not connected, or its hostname is outside `.localhost` or your `hostnames.tld`, so the box's hostnames were skipped.
- The request went to `localhost` or `127.0.0.1` without a lane hostname.
- Another server, such as Caddy or nginx, was meant to answer, but the daemon holds the port.

For clients on this machine requesting `localhost`, an IP address or a `.localhost` name, the page lists the routed hostnames and suggests the closest one. Other names get no list, so a website that re-resolves its domain to 127.0.0.1 cannot read your projects and branches.

## Fix

```sh
devlanes url ls          # routed hostnames with lane, service and state
devlanes config check
devlanes remote ls       # lanes on a box
devlanes doctor
```

To hand ports 80 and 443 to another server, stop the daemon, which stops every service it manages, or move the proxy with `proxy.http_port` and `proxy.https_port` in the user config:

```sh
devlanes daemon stop
```

[Hostnames and HTTPS](../guides/https.md) explains how hostnames are built.
