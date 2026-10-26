---
sidebar:
  order: 102
---

# DL102 Invalid host

`400`. The proxy cannot tell which hostname the request is for. It comes from hand-written requests, HTTP/1.0 clients and misconfigured proxies in front of devlanes.

## Causes

- No `Host` header and no absolute URL.
- Two `Host` headers.
- In HTTP/2, `Host` disagrees with `:authority`. In HTTP/1.1, an absolute URL wins over `Host`, as RFC 9112 requires.
- The host contains user information (`user@acme.localhost`) or a port that is not a number.

## Fix

Send one `Host` header that matches the URL. curl resolves `*.localhost` to loopback itself:

```sh
curl http://acme.localhost/
curl -H 'Host: acme.localhost' http://127.0.0.1/
```
