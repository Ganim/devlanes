---
sidebar:
  order: 203
---

# DL203 Service failed

`503`. The service exited, or its `ready` check failed or timed out. devlanes does not restart it.

The page shows the end of the service's output to clients on this machine. Other devices (`proxy.lan: true`) do not get it, because output can contain secrets. Box lanes show none; read the output on the box.

Common causes are a taken port (`EADDRINUSE`), often by a copy of the server started outside devlanes, a missing dependency or variable, and a `ready` timeout shorter than the first compile.

## Fix

```sh
devlanes debug why acme@2:web
devlanes logs acme@2:web --since start
devlanes debug errors                    # crashes and port conflicts in every lane
devlanes restart acme@2:web
```
