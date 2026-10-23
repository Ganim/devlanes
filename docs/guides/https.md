# Hostnames and HTTPS

Every service with `hosts` is reachable at `https://<host>` through the daemon's proxy. The lane sits in the registrable label (`api.acme-2.localhost`, never `api.lane2.acme.localhost`), so each lane is its own site for cookies and storage, like `api.acme.com` and `app.acme.com` share `acme.com` in production.

## The flow

1. The browser resolves `api.acme-2.localhost`. Chrome, Firefox and the macOS 26 and systemd-resolved resolvers answer `*.localhost` with loopback by themselves; nothing is configured.
2. It connects to port 443 (or 8443 when 443 could not be bound) and sends the hostname in the TLS handshake. The proxy looks the name up in its route table. An unknown name is refused at the handshake, so only registered hostnames ever get a certificate.
3. The proxy signs a leaf certificate for that name with the devlanes CA, valid 30 days, cached for 24 hours, and completes the handshake with HTTP/2 or HTTP/1.1.
4. The request goes to `127.0.0.1:<service port>` over HTTP/1.1 with `Host` preserved, `X-Forwarded-Proto: https`, `X-Forwarded-Host`, `X-Forwarded-For`, `X-Forwarded-Port` and `Forwarded`. WebSocket upgrades are bridged with no idle timeout.
5. Plain `http://` on port 80 (or 8080) answers known hostnames with a 308 to https.

A hostname whose lane is idle, whose service is starting or has failed gets a 503 page that says so, with the command to start it and, for failures, the end of the service's log. 404 means no route has that name; 508 means a service proxied a request back to devlanes without rewriting `Host`.

## Trust the CA once

```sh
devlanes certs trust
```

The CA is created on first use at `~/.devlanes/ca/ca.pem` (key `ca-key.pem`, mode 0600) and the daemon writes `~/.devlanes/ca/bundle.pem`, the system roots plus the CA. `certs trust` runs in your terminal, never in the daemon, because it needs your password (on Windows, a click in a dialog).

What it does per OS, by the stores in `tls.trust` (default `[system]`):

| OS | `system` | `chrome` | `firefox` |
| --- | --- | --- | --- |
| macOS | login keychain, trusted as root for your user (GUI password dialog, no sudo); Safari, Chrome, Edge and `/usr/bin/curl` read it; Firefox 120+ too | same store as `system` | `certutil` into every Firefox profile; needs `brew install nss` |
| Linux | anchor directory of your distro (`/usr/local/share/ca-certificates`, `/etc/pki/ca-trust/source/anchors`, …) then `update-ca-certificates` or `update-ca-trust`, through sudo | `~/.pki/nssdb` with `certutil`; Chrome, Chromium and Playwright's browsers read it | every profile under `~/.mozilla/firefox` (snap and flatpak paths too); needs `libnss3-tools` or `nss-tools` |
| Windows | your user root store, no administrator rights; Windows asks to confirm each change in a dialog; Chrome, Edge and `curl.exe` read it | same store as `system` | same store as `system` (Firefox reads it through `security.enterprise_roots.enabled`, on by default) |

On Linux the system store does not reach browsers, so set `tls.trust: [system, chrome, firefox]` in `~/.devlanes/config.yaml` there. Inside WSL the default adds `windows`, the Windows user root store that browsers on Windows read; see [Windows (WSL2)](wsl.md). `devlanes certs untrust` removes the CA from the same stores; `--keep-ca` with `--hosts` or `--resolver` removes only those extras.

`devlanes doctor` reports the CA path and, per store, whether it is trusted. The CA is constrained to `localhost`, `test`, `internal`, `home.arpa` and the TLD configured when it was created; a hostname outside those is reported by `doctor` as `outside the CA's names`.

### Programs started by devlanes

Services do not need the system store. Each unit gets `DEVLANES_CA_CERT` and `DEVLANES_CA_BUNDLE`, and the platform sets the variable its runtime reads: `NODE_EXTRA_CA_CERTS` (Node, Bun), `DENO_CERT`, `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE` (Python, Rust). A service calling `https://auth.acme-2.localhost` from its own code therefore works without any TLS workaround. Remove `NODE_TLS_REJECT_UNAUTHORIZED=0` and hand-rolled `NODE_EXTRA_CA_CERTS` from dev scripts; a script that overwrites the variable loses the CA.

Other tools from your shell:

| Tool | Reads | Do |
| --- | --- | --- |
| Homebrew curl, OpenSSL CLI | `SSL_CERT_FILE`, `CURL_CA_BUNDLE` | `export SSL_CERT_FILE=~/.devlanes/ca/bundle.pem`, or `devlanes env shell` |
| Go binaries | the OS store on macOS; `SSL_CERT_FILE` on Linux | nothing on macOS |
| Java | its own `cacerts` | `keytool -importcert -file ~/.devlanes/ca/ca.pem` |
| Docker containers | nothing from the host | mount `ca.pem` and run `update-ca-certificates`, or talk to services by `host.docker.internal:<port>` over http |
| iOS simulator, Android emulator | own stores | not handled yet; `tls.trust` accepts the names and ignores them with a warning |

On Windows, `curl.exe` needs `--ssl-revoke-best-effort` because the devlanes CA publishes no revocation list, and Python's `requests` reads `REQUESTS_CA_BUNDLE` rather than `SSL_CERT_FILE`; [Windows](windows.md#https) has the commands.

### mkcert

`tls.ca: mkcert` makes the daemon load mkcert's `rootCA.pem` from `mkcert -CAROOT` instead of creating its own CA. Browsers that already trust mkcert keep working and `certs trust` has nothing to install. mkcert's root has no name constraints and `certs untrust` leaves it alone (`mkcert -uninstall` removes it).

## Hostnames in config

```yaml
services:
  api:
    hosts:
      - api.${{ lane.domain }}
      - { host: "*.${{ lane.domain }}", fallback: true }          # used only when no other route matches
      - { host: "inspect.${{ lane.domain }}", port: inspector }   # route to a named port
      - { host: "legacy.${{ lane.domain }}", host_header: upstream } # send 127.0.0.1:<port> as Host
infra:
  hosts:
    minio: ["s3.${{ lane.domain }}"]                              # a container's published port
```

`lane.domain` is `acme.localhost` in lane 0 and `acme-<N>.localhost` elsewhere; keep every hostname under it or under a `lanes.vars` value built the same way, so lanes never share a name. Matching is exact first, then wildcards (longest first), then fallbacks. A wildcard covers any depth below its parent but not the parent itself; list the apex too when you need it. A name claimed by two lanes goes to the one with running units and both get a `host_conflict` warning.

`hosts` also change the default readiness check to `tcp` on the service's port.

Tenant-style subdomains (`*.pages-2.localhost`) are fine as long as the lane index is in the label: `"*.pages-${{ lane.index }}.localhost"` in the full example. Browsers reject a wildcard directly under a TLD (`*.localhost`), and so does devlanes.

## Programs that cannot resolve *.localhost

workerd (Wrangler), musl binaries, pure-Go programs built without cgo, Safari before macOS 26 and anything inside a container send `*.localhost` to DNS. Two answers:

```sh
devlanes certs trust --hosts
```

installs a small root-owned helper (`/usr/local/libexec/devlanes-hosts`) with a sudoers rule for it alone, and the daemon then keeps a `# devlanes begin` / `# devlanes end` block in `/etc/hosts` with the exact hostnames of projects that want it. Wildcards cannot be written there. The user setting `hostnames.hosts_file` is `auto` by default, which writes names only when a startup probe shows the system resolver fails `*.localhost`; `lanes.hosts_file: always` in a project forces it for that project (Wrangler repos want this), `never` in the user config wins over every project. `crates/hosts-helper/README.md` describes what the helper accepts and refuses. On Windows, which resolves only `localhost` itself, `--hosts` installs the `devlanes-hosts` service instead, after an administrator prompt; see [Windows](windows.md#hostnames-for-programs-other-than-browsers).

```yaml
# ~/.devlanes/config.yaml
hostnames: { resolver: resolver, tld: test }
```

```sh
devlanes certs trust --resolver
```

switches lane domains to `acme.test` and `acme-2.test`, answered by a DNS server the daemon runs on `127.0.0.1:15353`. `--resolver` writes `/etc/resolver/test` on macOS, or a systemd-resolved delegate (systemd 258 or newer) on Linux, so the OS sends only that TLD there.

On Windows the server listens on `127.53.0.1:53`, because a Name Resolution Policy Table (NRPT) rule names a server by address only, and `--resolver` adds an NRPT rule for `.test` after the administrator prompt. In their default automatic secure DNS mode, Chrome and Edge do not upgrade lookups to DNS over HTTPS while any NRPT rule exists, so `*.test` reaches the rule; with a secure DNS provider chosen explicitly those names fail, so pick the OS default provider or turn secure DNS off. Group Policy NRPT rules make Windows ignore local ones, and `doctor` says so. `crates/resolver-dns/README.md` has the details.

The TLD must be `test`, `internal`, `home.arpa` or a name under one of them. The CA is constrained to the TLD it was created with; switching later needs the recreate command `doctor` prints. `.dev` is a real TLD with HSTS preloaded and `.local` is mDNS; neither is accepted.

## Ports 443 and 80

macOS lets a user process bind the low ports on the wildcard address; devlanes does so and accepts only loopback peers unless `proxy.lan: true`. On Linux the daemon falls back to 8443 and 8080 unless it may bind low ports:

```sh
sudo setcap cap_net_bind_service=+ep "$(command -v devlanes)"   # redo after upgrades
# or
sudo sysctl -w net.ipv4.ip_unprivileged_port_start=80
```

Windows needs no privilege for either port, but http.sys often holds port 80 there (`netsh http show servicestate`), and the daemon then uses 8080.

A fallback is a `doctor` warning, and public URLs then carry the port: `https://api.acme-2.localhost:8443`. `proxy.https_port` and `proxy.http_port` set other ports on purpose.

## Access from other devices

`proxy.lan: true` accepts connections from the network; on Windows, Defender Firewall asks the first time whether `devlanes.exe` may. Devices still have to resolve the hostnames to your machine (their own hosts file, or a DNS server pointing `*.test` at your IP with the resolver strategy) and trust the CA. Mirroring a box's lanes to a laptop does not need this; see [remote boxes](../remote.md).

## Checking

```sh
devlanes url ls                                   # routed hostnames with lane, unit, port and state
curl -sS https://api.acme-2.localhost/healthz     # works without -k once the CA is trusted
curl -sS --cacert ~/.devlanes/ca/ca.pem https://api.acme-2.localhost/healthz
devlanes doctor                                   # proxy ports, CA, trust per store, hosts file
```

`ERR_CERT_AUTHORITY_INVALID` in a browser means that browser's store does not have the CA: Firefox and Chromium on Linux need their own entries in `tls.trust`. A certificate error for a name that `url ls` lists under another lane means two lanes claim it. `curl: (35)` or `handshake failure` on a name `url ls` does not show means the service has no `hosts` entry for it.
