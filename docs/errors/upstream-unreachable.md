---
sidebar:
  order: 301
---

# DL301 Service not listening

`503`. devlanes has the service as running, but nothing accepted connections on its port. The proxy tries `127.0.0.1`, then `::1`. It retries refused connections for 5 seconds, so a server that a file watcher restarts gets the request once it is back.

## Causes

- The service picks its own port instead of reading `PORT`.
- It listens only on a network address such as `192.168.1.20`.
- It crashed and devlanes has not recorded the exit yet; the next request gets [DL203](service-failed.md).
- Its `ready` check passed before it listened, for example with `ready: started`.

## Fix

Compare the routed port with the one the service prints when it starts:

```sh
devlanes url ls
devlanes logs acme@2:web --tail 100
devlanes debug why acme@2:web
```

The [framework presets](../guides/presets.md) pass the port and a loopback host to Vite, Next.js and Wrangler.
