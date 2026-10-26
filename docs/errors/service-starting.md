---
sidebar:
  order: 202
---

# DL202 Service starting

`503`. A request to a starting service waits until its `ready` check passes and is then forwarded. This page appears when that takes more than 30 seconds, or sooner if the service fails, which gives [DL203](service-failed.md).

## Causes

- The check cannot pass: an `http` path the app does not serve, a `log` pattern it never prints, or a `tcp` port it does not open.
- The service listens on a fixed port instead of `PORT`.
- It waits for something that is not up, such as a database.

## Fix

```sh
devlanes wait acme@2:web          # exits 0 when ready, 1 when failed, 124 on timeout
devlanes debug why acme@2:web
devlanes logs acme@2:web --tail 100
```

The `ready` options are in the [config reference](../reference/config.md).
