# devlanes

devlanes runs a project's dev environment once per Git worktree on macOS and Linux. Each worktree is a lane with its own ports, hostnames and data; a per-user daemon owns the processes and the CLI and TUI are its clients. devlanes is pre-alpha.

## Install

```sh
brew install devlanes-run/tap/devlanes
npm install -g devlanes
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes
curl -fsSL https://raw.githubusercontent.com/devlanes-run/devlanes/main/install.sh | sh
```

Each of these installs `devlanes` and `devlanes-hosts` from a GitHub release for macOS or Linux (glibc), arm64 or x86_64; cargo binstall needs a second run with `devlanes-hosts-helper` for the helper. No release has been tagged yet, so until the first one only the source build works:

```sh
cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes devlanes-hosts-helper
```

`install.sh` verifies the archive against the release's `SHA256SUMS`, installs to `~/.local/bin` (`DEVLANES_INSTALL_DIR` changes it) and takes `DEVLANES_VERSION=X.Y.Z` to pin a release. [packaging](packaging/README.md) describes the release pipeline.

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

The commands are listed in [bins/devlanes](bins/devlanes/README.md). See [design](docs/design.md), [development conventions](AGENTS.md), [fixtures](fixtures/README.md), [the agent plugin](agent-plugin/README.md) and [M0 commands](docs/milestones/M0.md). mprocs and dekit informed terminal interaction; pitchfork, process-compose, devenv, Coasts, portless, kache, overmind and Conductor informed process and worktree workflows.

## Config example

```yaml
# Project name shared by every worktree of this repository.
project: myapp
# Long-running processes, started together after infra and setup.
services:
  api:
    # Runs a script from package.json with the project's package manager.
    run_script: api/package.json#dev
    # HTTPS hostnames routed by the proxy; lane.domain is myapp.localhost or myapp-<N>.localhost.
    hosts: ["api.${{ lane.domain }}"]
  web:
    # Adds Vite's port and host flags to the command.
    preset: vite
    run_script: web/package.json#dev
    # Extra variables for this service; public_url is the API's https:// address in this lane.
    env: { VITE_API_URL: "${{ svc.api.public_url }}" }
    hosts: ["${{ lane.domain }}"]
```

In the worktree of lane 2 this serves `https://api.myapp-2.localhost` and `https://myapp-2.localhost`, each on its own allocated port. The excerpt comes from [minimal.yaml](docs/examples/minimal.yaml), which also has shared Postgres and a database per lane; [devlanes.yaml](docs/examples/devlanes.yaml) shows every form.

## License

Apache-2.0. See [LICENSE](LICENSE).
