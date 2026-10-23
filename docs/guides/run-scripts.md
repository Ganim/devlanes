# Run and run_script

A service or task needs exactly one of `run` and `run_script`. `run_script` names a script in a project file (`package.json`, `Makefile`, `justfile`, `deno.json`, `Cargo.toml`, `pyproject.toml`) and devlanes builds the command for you. `run` is a command devlanes runs exactly as written.

Use `run_script` whenever the command is a script in one of those files. devlanes then knows what it runs, which `run` text cannot tell it:

- Presets work only with `run_script`. The runner places their flags after the script name in the form the package manager needs, so `preset: vite` on a pnpm, yarn, bun or npm script gets `--port` where Vite sees it.
- The script must exist before anything starts; a typo fails with the list of scripts the file has.
- The package manager comes from the project (`packageManager` or the lockfile), not from what you typed.
- Profiles can swap the script (`run_scripts: { dev: start }`).
- Wrangler can refuse a script that hardcodes `--port`.
- The process starts in the file's directory and platform detection reads the file.

Use `run` for everything else: a binary, a pipeline, a multi-line script, a wrapper such as `turbo`, or a tool with no script file. `infra.setup` and `infra.teardown` take only `run`, since those commands are written out on purpose (`docker compose exec -T postgres ...`).

## run

```yaml
services:
  api:
    run: pnpm --filter api dev                  # a string runs through /bin/sh -c
  worker:
    run: [node, worker.mjs, --concurrency, "4"] # a list is an argv, no shell
  certs:
    run: |                                      # a multi-line string is a shell script
      set -euo pipefail
      mkdir -p .devlanes/certs
      ./scripts/gen-certs.sh .devlanes/certs
```

`run` starts in the lane's worktree root, or in `cwd` when set. Expressions work in both forms: `run: uvicorn app:main --port ${{ self.port }}`.

devlanes never edits a `run` command, so it cannot be combined with `preset`: `devlanes config check` refuses the pair. To keep a hand-written command, drop the preset and add its flags yourself; [presets](presets.md) lists what each one adds.

## run_script

```yaml
services:
  api:
    run_script:
      file: apps/api/package.json
      name: dev
tasks:
  fixtures:
    run_script:
      file: justfile
      name: fixtures
```

`run_script` is a mapping with two keys, both required:

- `file`: the file that defines the script, relative to the worktree root (not to the directory of `devlanes.yaml`). It must stay inside the worktree. Its name picks the runner (table below).
- `name`: the script, target, recipe, task or binary in that file.

A string such as `run_script: apps/api/package.json#dev` is refused with the mapping to write instead. The command always runs in the directory of `file`, so `devlanes config check` refuses a `cwd` on the same unit, including one inherited through `extends`. `${{ }}` is not evaluated in `file` or `name`.

## Runners

The first runner whose file name matches wins, in this order.

| File | Runner | Command built |
| --- | --- | --- |
| `package.json` | npm | `<pm> run <name>`; the package manager is detected |
| `GNUmakefile`, `makefile`, `Makefile`, `*.mk`, `*.mak` | make | `make [-f <file>] <target>` |
| `justfile`, `.justfile` (any case), `*.just` | just | `just [--justfile <file>] <recipe>` |
| `deno.json`, `deno.jsonc` | deno | `deno task [--config deno.jsonc] <name>` |
| `Cargo.toml` | cargo | `cargo run [-p <member>] --bin <name>` |
| `pyproject.toml` | python | `uv run <name>` or `poetry run <name>` |

A file nobody handles (`requirements.txt`, `Taskfile.yml`, `setup.py`) fails with `no script runner handles <file>`. Use `run` for those.

Every runner checks that the script exists before anything starts and lists what it found instead: `` `apps/api/package.json` has no script `dev` (available: build, start) ``.

### package.json

The package manager is picked from, in order:

1. `packageManager` (or `devEngines.packageManager`) in the nearest `package.json`, walking up to the workspace root. `pnpm@10.4.1+sha512...` is read as pnpm.
2. The nearest lockfile, up to the worktree root: `pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `bun.lock`, `bun.lockb`.
3. npm, with a note suggesting you add `packageManager`.

Two lockfiles of different managers in one directory is an error; delete the stale one or set `packageManager`. Yarn 1 and Yarn 2+ are told apart by the declared version, the `yarn.lock` header, then `.yarnrc.yml`.

Preset flags are appended after the script name. Only npm gets a `--` separator (`npm run dev -- --port 20012`); pnpm, yarn and bun forward a literal `--` to the script, which makes Vite ignore what follows, so they get `pnpm run dev --port 20012`.

`devlanes env show api --explain` prints the detection as a note, for example `npm: pnpm 10.4.1 (packageManager in package.json)`.

### Makefile

Targets are checked with a conservative parser. Files that `include` others, use pattern rules or `$(eval` are not checked and the target runs anyway.

make cannot take extra arguments, so a preset on a Makefile service fails at start: `make cannot pass arguments to target dev`. Put the framework command in a package.json script or a justfile recipe instead, or drop the preset and use `run` with the flags written out.

### justfile

Recipes and aliases are followed; `import` and `mod` make the file unverifiable, so the recipe runs unchecked. A preset's flags need a variadic last parameter:

```just
dev *ARGS:
    vite {{ARGS}}
```

A recipe with required positional parameters cannot be used, since `run_script` has nothing to pass. Give them defaults. Recipes declared with `+ARGS` (one or more) fail the same way, even with a preset; use `*ARGS`.

### deno.json

`.json` files are read as JSONC too, so comments and trailing commas are fine. Tasks may be strings or objects with `command`. A `package.json` always goes to the npm runner, even in a Deno project; point `run_script` at `deno.json` to get `deno task`.

### Cargo.toml

`{ file: Cargo.toml, name: server }` runs `cargo run --bin server`. From a workspace root it finds the member that owns the binary and adds `-p <member>`; a binary defined in two members is an error asking you to point at the member's manifest. Binaries come from `src/main.rs`, `src/bin/*.rs`, `src/bin/*/main.rs` and `[[bin]]`. There is no `--release`, `--example` or `--features`; use `run: cargo run --release --bin server` for those.

### pyproject.toml

The name is looked up in `[project.scripts]`, then `[tool.poetry.scripts]`. uv is used when a `uv.lock` exists (from the project directory up to the worktree) or the file has a `[tool.uv]` table; poetry when `poetry.lock` sits next to the file. Neither is an error asking you to run `uv lock` or `poetry lock`. pdm, hatch, rye and pipenv are not handled; use `run`.

Console scripts are entry points, so `run_script` fits a `serve = "acme.server:main"` entry. For `uvicorn app:main --reload` or `manage.py runserver`, use `run`; see [languages](languages.md#python).

## Profiles that swap scripts

```yaml
profiles:
  e2e:
    include: [+api, +web]
    run_scripts:
      dev: start
```

Every service whose `run_script` names `dev` runs `start` from the same file instead. The map is a single lookup (no chaining) and applies to services only; tasks keep their names. With several `--profile` flags, the later profile wins per key. A remapped name that does not exist fails at start with the runner's missing-script error.

## Overrides and templates

`run_script` merges per key, so a local override or a template child can change only the name:

```yaml
# devlanes.local.yaml
services:
  api:
    run_script:  # keeps the inherited file
      name: dev:debug
```

Setting `run` clears an inherited `run_script` and the reverse; for tasks, `steps` clears both.
