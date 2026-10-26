---
sidebar:
  order: 302
---

# DL302 Upstream request failed

`502`. The service accepted the connection, then closed or reset it, or answered with something other than an HTTP/1.1 response. The proxy does not resend a request that may have reached the service.

## Causes

- The service serves HTTPS on its port, such as Vite with `server.https`. devlanes terminates TLS and sends plain HTTP/1.1.
- It speaks only HTTP/2 without TLS (h2c), such as many gRPC servers.
- It crashed while handling the request.
- Something else now holds the port, such as a database.

## Fix

Serve plain HTTP/1.1 on the service's port, then check its output:

```sh
devlanes logs acme@2:web --tail 100
devlanes debug errors --since 5m
```
