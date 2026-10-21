# devlanes

devlanes manages development environments for macOS and Linux. Each Git worktree becomes a lane with its own ports, hostnames and data, and a per-user daemon runs the services.

```sh
npm install -g devlanes
devlanes --help
```

This package holds a small launcher. npm installs the native binary from one of the `@devlanes/<platform>-<arch>` packages, picked by `os`, `cpu` and `libc`. Builds exist for macOS arm64 and x64 and for Linux arm64 and x64 with glibc.

Documentation: https://devlanes.run. Source: https://github.com/devlanes-run/devlanes.
