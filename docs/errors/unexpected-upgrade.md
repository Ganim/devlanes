---
sidebar:
  order: 303
---

# DL303 Unexpected protocol upgrade

`502`. The service answered `101 Switching Protocols` to a request that did not ask for an upgrade, which the client cannot accept. WebSocket requests ask for one and are not affected. This is a bug in the service or one of its libraries.

## Fix

Check the service's upgrade handling and output:

```sh
devlanes logs acme@2:web --tail 100
```

If devlanes is at fault, [open an issue](https://github.com/devlanes-run/devlanes/issues/new) with a request that reproduces it.
