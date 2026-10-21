# devlanes

devlanes manages development environments for macOS and Linux. Each Git worktree becomes a lane with its own ports, hostnames and data, and a per-user daemon runs the services.

## Usage

```sh
npm install -g devlanes
devlanes --help
```

Documentation: https://devlanes.run. Source: https://github.com/devlanes-run/devlanes.

## Shorter name

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

## Config schemas

The package ships the JSON Schemas of the config files, exported as `devlanes/schema.json` (for `devlanes.yaml` and `devlanes.local.yaml`) and `devlanes/user-schema.json` (for `~/.devlanes/config.yaml`). With devlanes installed in the project (`npm install -D devlanes`), this first line in `devlanes.yaml` makes editors that use yaml-language-server validate and complete the file:

```yaml
# yaml-language-server: $schema=./node_modules/devlanes/schema.json
```

yaml-language-server resolves the path from the YAML file's directory, so it fits a `devlanes.yaml` next to `node_modules`.

## Behaviour

This package holds a small launcher. npm installs the native binary from one of the `@devlanes/<platform>-<arch>` packages, picked by `os`, `cpu` and `libc`. Builds exist for macOS arm64 and x64 and for Linux arm64 and x64 with glibc.

## License

Apache-2.0
