---
sidebar:
  order: 204
---

# DL204 Box reconnecting

`503`. The hostname belongs to a lane on a remote box and the connection to the box is down. The daemon reconnects with backoff after network loss or sleep. Requests wait up to 15 seconds for the reconnect and are then forwarded; this page appears when it takes longer.

When the connection fails for good, such as rejected credentials or a box name that does not resolve, the box's hostnames are removed and requests get [DL101](unknown-host.md).

## Fix

```sh
devlanes remote ls                  # state and last error of each box
devlanes remote connect my-box      # shows SSH prompts in this terminal
devlanes daemon logs --tail 50
```

On a Linux box, `sudo loginctl enable-linger "$USER"` keeps its daemon running after you log out. See [remote boxes](../remote.md).
