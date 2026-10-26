---
sidebar:
  order: 104
---

# DL104 Proxy loop

`508`. The proxy adds the route's hostname to `x-devlanes-via` on every request it forwards. A request that arrives with its own route's hostname in that header has come back from the service, and forwarding it again would never end.

## Causes

A dev server proxy or rewrite sends requests to devlanes, for example to `http://localhost` or the app's own `https://` URL, and keeps the original `Host` header, so the proxy routes them to the same service. Vite `server.proxy` without `changeOrigin` and Next.js `rewrites` to the app's own URL do this. Forwarding to a different devlanes hostname is not a loop.

## Fix

Point the proxy at the target service's port. `${{ services.api.port }}` in `devlanes.yaml` gives a service another service's port:

```yaml
services:
  web:
    run_script: { file: package.json, name: dev }
    env:
      API_URL: "http://127.0.0.1:${{ services.api.port }}"
```

```ts
// vite.config.ts
server: { proxy: { '/api': process.env.API_URL } }
```

If the target has to stay a devlanes hostname, rewrite `Host` to it (Vite `changeOrigin: true`). See [ports and environment](../guides/ports-and-env.md).
