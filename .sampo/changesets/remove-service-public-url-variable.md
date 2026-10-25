---
cargo/devlanes: minor
cargo/devlanes-config: minor
cargo/devlanes-resolve: minor
---

Removed the `DEVLANES_PUBLIC_URL` variable that services received.

Apps that read it get nothing now. Set it in the service's `env` instead:

```yaml
env:
  DEVLANES_PUBLIC_URL: ${{ self.public_url }}
```

`DEVLANES_PUBLIC_URL_<SERVICE>` in `with`, `exec` and `env show` is unchanged.
