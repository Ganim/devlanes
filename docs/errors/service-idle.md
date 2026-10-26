---
sidebar:
  order: 201
---

# DL201 Service not running

`503`. The hostname belongs to a stopped service: its lane is stopped, or the service was stopped or left out of the last `devlanes start`. Routes of stopped lanes stay registered, so the page can name the lane.

## Fix

```sh
devlanes start --lane acme@2      # the whole lane, as the page shows
devlanes start acme@2:web         # only this service
devlanes debug why acme@2:web
```

A box lane is started on the box; the connection only mirrors it.
