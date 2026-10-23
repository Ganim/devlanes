# devlanes

devlanes runs a project's dev environment once per Git worktree on macOS, Linux and Windows. Each worktree is a lane with its own ports, hostnames and data; a per-user daemon owns the processes and the CLI and TUI are its clients. devlanes is pre-alpha.

## Install

```sh
brew install devlanes-run/tap/devlanes
npm install -g devlanes
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes
curl -fsSL https://devlanes.run/install.sh | sh
```

These install release builds for macOS or Linux (glibc), arm64 or x86_64; Windows is below. Homebrew, npm and `install.sh` install both `devlanes` and the `devlanes-hosts` helper; cargo binstall installs `devlanes`, and `cargo binstall --git https://github.com/devlanes-run/devlanes devlanes-hosts-helper` adds the helper. To build from source instead:

```sh
cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes devlanes-hosts-helper
```

`install.sh` verifies the archive against the release's `SHA256SUMS`, installs to `~/.local/bin` (`DEVLANES_INSTALL_DIR` changes it; `DEVLANES_INSTALL_SUDO=1` allows sudo for a directory you can't write) and takes `DEVLANES_VERSION=X.Y.Z` to pin a release. [packaging](packaging/README.md) describes the release pipeline.

On Windows 11 24H2 or later, x64 or arm64, install from PowerShell:

```powershell
irm https://github.com/devlanes-run/devlanes/releases/latest/download/install.ps1 | iex
```

`install.ps1` verifies the archive the same way, installs to `%LOCALAPPDATA%\Programs\devlanes` and adds it to your user `PATH`. `npm install -g devlanes` works too, and cargo binstall needs one run each for `devlanes`, `devlanes-logon-launcher` and `devlanes-hosts-service`. devlanes on Windows needs [Git for Windows](https://gitforwindows.org); [Windows](docs/guides/windows.md) covers setup, and [Windows (WSL2)](docs/guides/wsl.md) running the Linux build in WSL 2.

### Shorter name

devlanes ships no `dl` command. To type `dl`, add an alias to your shell config, after the line that loads devlanes completions, so the alias completes like `devlanes`:

```sh
# zsh, ~/.zshrc
alias dl=devlanes
compdef dl=devlanes

# bash, ~/.bashrc
alias dl=devlanes
complete -o nospace -o bashdefault -F _clap_complete_devlanes dl

# fish, ~/.config/fish/config.fish (fish aliases inherit the command's completions)
alias dl devlanes
```

## Usage

```sh
devlanes start              # infra, setup, tasks, then services of this worktree's lane
devlanes status
devlanes logs api -f
devlanes                    # the TUI
```

[Getting started](docs/getting-started.md) walks from install to two lanes. The [CLI reference](docs/reference/cli.md) and the [config reference](docs/reference/config.md) are generated from the code, and tests fail when they fall behind.

`devlanes --help` lists the commands, and `devlanes <command> --help` describes each one. See [design](docs/design.md), [development conventions](AGENTS.md), [fixtures](fixtures/README.md), [the agent plugin](agent-plugin/README.md) and [M0 commands](docs/milestones/M0.md). mprocs and dekit informed terminal interaction; pitchfork, process-compose, devenv, Coasts, portless, kache, overmind and Conductor informed process and worktree workflows.

## Config example

```yaml
# Project name shared by every worktree of this repository.
project: acme
# Long-running processes, started together after infra and setup.
services:
  api:
    # Runs a script from package.json with the project's package manager.
    run_script: api/package.json#dev
    # HTTPS hostnames routed by the proxy; lane.domain is acme.localhost or acme-<N>.localhost.
    hosts: ["api.${{ lane.domain }}"]
  web:
    # Adds Vite's port and host flags to the command.
    preset: vite
    run_script: web/package.json#dev
    # Extra variables for this service; public_url is the API's https:// address in this lane.
    env: { VITE_API_URL: "${{ svc.api.public_url }}" }
    hosts: ["${{ lane.domain }}"]
```

In the worktree of lane 2 this serves `https://api.acme-2.localhost` and `https://acme-2.localhost`, each on its own allocated port. The excerpt comes from [minimal.yaml](docs/examples/minimal.yaml), which also has shared Postgres and a database per lane; [devlanes.yaml](docs/examples/devlanes.yaml) shows every form.

## License

Apache-2.0. See [LICENSE](LICENSE).
