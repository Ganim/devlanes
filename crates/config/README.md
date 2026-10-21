# devlanes-config

Parses and validates devlanes project and user YAML configuration, with source-labelled errors and JSON Schema generation. The daemon loads configuration through this crate; the resolver supplies runtime values for its MiniJinja expressions, and the composition root registers preset parsers and schemas.

## Usage

```rust
use devlanes_config::{ConfigError, parse_project, validate};

fn main() -> Result<(), ConfigError> {
    let project = parse_project(
        "project: demo\nservices:\n  api: { run: server }\n",
        Some("services:\n  api: { autostart: false }\n"),
    )?;
    let report = validate(&project)?;
    assert_eq!(report.project, "demo");
    Ok(())
}
```

Use `load_project(path)` for a file or directory. It also reads the sibling `devlanes.local.yaml`; loading is synchronous, so async callers should use a blocking worker.

## API

- `ProjectConfig` and `UserConfig`: separate typed project and machine settings.
- `check(path)`: load and validate a project, returning warnings or a `ConfigError`.
- `evaluate_typed`, `evaluate_env`, and `resolve_lane_vars`: resolve expressions using a caller-supplied `ExpressionContext`.
- `validate_with_presets`: pass opaque preset option objects to registered integration parsers.
- `project_schema_with_presets`: generate a project schema with integration-owned option schemas; `project_schema()` omits registration.
- `user_schema()`: generate the separate user configuration schema.

## Merge and expression rules

Local overrides merge before `extends` resolution. Top-level maps merge by key. Inside a service, `ready`, commands, `stop`, host/sync/tag lists, `cwd`, and each preset option block replace as a whole; `env` merges by key. Setting `run` replaces an inherited `run_script`, and vice versa. Task declaration order is preserved.

`${{ env.X }}` reads the supplied outer environment, including inside an env override map. Lane variables can reference other lane variables; cycles are rejected. Parsing does not read the process environment or run integration commands.

## License

Apache-2.0
