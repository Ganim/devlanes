# Run and run_script

A service, task or infra command needs exactly one of `run` and `run_script`. `run` is a command; `run_script` names a script in a file and lets devlanes build the command for you. The file decides which runner is used.

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

## run_script

```yaml
services:
  api:
    run_script: apps/api/package.json#dev
  fixtures:
    run_script: { source: justfile, name: fixtures }
```

The shorthand splits on the last `#`. `source` is relative to the worktree root (not to the directory of `devlanes.yaml`), and it must stay inside the worktree. The command always runs in the directory of the source file; a `cwd` on the same unit is ignored.

`run_script` values are plain strings. `${{ }}` is not evaluated in them.

Prefer `run_script` over `run: pnpm -F pkg dev` for anything a preset touches: the runner knows where the preset's flags go, and profiles can swap the script name (`run_scripts: { dev: start }`).

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

make cannot take extra arguments, so a preset on a Makefile service fails at start: `make cannot pass arguments to target dev`. Put the framework command in a package.json script or a justfile recipe instead, or use `run`.

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

`Cargo.toml#server` runs `cargo run --bin server`. From a workspace root it finds the member that owns the binary and adds `-p <member>`; a binary defined in two members is an error asking you to point at the member's manifest. Binaries come from `src/main.rs`, `src/bin/*.rs`, `src/bin/*/main.rs` and `[[bin]]`. There is no `--release`, `--example` or `--features`; use `run: cargo run --release --bin server` for those.

### pyproject.toml

The name is looked up in `[project.scripts]`, then `[tool.poetry.scripts]`. uv is used when a `uv.lock` exists (from the project directory up to the worktree) or the file has a `[tool.uv]` table; poetry when `poetry.lock` sits next to the file. Neither is an error asking you to run `uv lock` or `poetry lock`. pdm, hatch, rye and pipenv are not handled; use `run`.

Console scripts are entry points, so `run_script` fits a `serve = "acme.server:main"` entry. For `uvicorn app:main --reload` or `manage.py runserver`, use `run`; see [languages](languages.md#python).

## Profiles that swap scripts

```yaml
profiles:
  e2e:
    include: [+api, +web]
    run_scripts: { dev: start }
```

Every service whose `run_script` names `dev` runs `start` from the same file instead. The map is a single lookup (no chaining) and applies to services only; tasks and infra commands keep their names. With several `--profile` flags, the later profile wins per key. A remapped name that does not exist fails at start with the runner's missing-script error.

## Overrides and templates

`run_script` in object form merges per key, so a local override or a template child can change only the name:

```yaml
# devlanes.local.yaml
services:
  api:
    run_script: { name: dev:debug }   # keeps the inherited source
```

The shorthand replaces the whole value. Setting `run` clears an inherited `run_script` and the reverse; for tasks, `steps` clears both.
