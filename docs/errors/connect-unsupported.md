---
sidebar:
  order: 103
---

# DL103 CONNECT not supported

`405`. A client sent a CONNECT request for a routed hostname. devlanes only forwards requests to its own hostnames and does not open tunnels.

## Causes

- `HTTP_PROXY`, `HTTPS_PROXY` or a tool's proxy setting points at a devlanes hostname or at port 80 or 443 of this machine.
- A WebSocket client that requires HTTP/2 extended CONNECT. Browsers fall back to an HTTP/1.1 Upgrade, which works, because the proxy does not offer extended CONNECT.

## Fix

```sh
env | grep -i _proxy
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
```
