# devlanes docs

Start with [getting started](getting-started.md). The two references are generated from the code and are the authority on fields and flags; the guides explain how the pieces fit and show configurations that work.

## Reference

- [CLI](reference/cli.md): every command and option.
- [Config](reference/config.md): every field of `devlanes.yaml` and the user `config.yaml`.
- [Examples](examples/README.md): commented configuration files.
- [Proxy errors](errors/README.md): every error page the proxy serves, its causes and fixes.

## Guides

- [Run and run_script](guides/run-scripts.md): the six script runners, which file picks which, and how presets pass their flags.
- [Languages](guides/languages.md): Node, Bun, Deno, Python and Rust services, mise and direnv.
- [Framework presets](guides/presets.md): Vite, Next, Wrangler and Tauri.
- [Ports and environment](guides/ports-and-env.md): port blocks, named ports, `PORT`, expressions, env layers, `.env`.
- [Hostnames and HTTPS](guides/https.md): the proxy, the CA, trusting it per OS and runtime, resolver strategies, LAN access.
- [Infra](guides/infra.md): Compose and plain containers, shared versus per-lane, recipes for Postgres, MySQL, Redis, RabbitMQ, MinIO and Mailpit.
- [Service patterns](guides/services.md): APIs behind a frontend, workers, several ports, wildcards, replicas, lifecycle tasks, CI.
- [Terminals](guides/terminals.md): tab progress bars, clipboard.
- [Remote setup](guides/remote-setup.md): preparing a box and a client for `devlanes remote connect`.
- [Remote boxes over Tailscale](guides/remote-tailscale.md): MagicDNS names, OpenSSH keys or Tailscale SSH, check mode, Headscale.
- [Remote boxes through Cloudflare](guides/remote-cloudflare.md): Cloudflare Tunnel with Access and `cloudflared` as `ProxyCommand`, the Cloudflare One Client, session expiry.
- [Windows](guides/windows.md): requirements, Git for Windows' shell, stops, ports, HTTPS, hostnames and logon start on Windows 11.
- [WSL](guides/wsl.md): devlanes inside a WSL distro with the browser on Windows: trusting the CA in Windows, opening URLs, systemd, worktrees on Windows drives.

## Other

- [Remote boxes](remote.md): what mirroring does once connected.
- [Design](design.md): lanes, ports, the daemon and startup order.
- [Observability](observability.md): spans, diagnostics and plans for tracing.
