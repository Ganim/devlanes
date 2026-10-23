# devlanes-run-script-make

Runs `run_script: { file: Makefile, name: <target> }` as `make <target>` in the Makefile's directory. Lane resolution calls it through `MakeTargets`, a `devlanes-core` `ScriptRunner`.

## Usage

```rust
use std::path::Path;
use devlanes_run_script_make::{TargetRequest, plan};

let launch = plan(&TargetRequest {
    worktree: Path::new("/lanes/acme-1"),
    source: Path::new("infra/Makefile"),
    name: "certs",
    extra_args: &[],
})?;
assert_eq!(launch.argv, ["make", "certs"]);
```

The composition root registers `Arc::new(MakeTargets)` as a script runner. `plan` reads the Makefile, so the adapter runs it on a blocking thread.

## API

- `plan`: checks the target and renders the command. `-f <file>` is added only when make would pick another file in that directory (`GNUmakefile` before `makefile` before `Makefile`), or for `*.mk` and `*.mak` files.
- `parse` and `Targets::check`: the target check on its own, returning `Found`, `Missing` or `Unverifiable`.
- `handles`: which source files this runner accepts.

## Config example

```yaml
tasks:
  certs:
    # Runs the certs target of infra/Makefile with make.
    run_script:
      file: infra/Makefile
      name: certs
```

`devlanes run certs` then runs `make certs` in `infra/`. Services with a preset cannot use a Makefile target, because make cannot pass the preset's flags.

## Behaviour

A target is reported missing only when nothing in the file could define it out of the parser's sight. Includes, pattern rules, variable target names, `$(eval)`, `.DEFAULT` and `.RECIPEPREFIX` make the result `Unverifiable`, and make decides at run time. Built-in implicit rules, such as building `foo` from `foo.c`, are not considered.

make has no way to pass arguments to a recipe, so a request with `extra_args` (preset flags, for example) is an error.

## License

Apache-2.0
