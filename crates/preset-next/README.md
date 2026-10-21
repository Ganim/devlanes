# devlanes-preset-next

`preset: next` binds `next dev` to the service's allocated port on loopback. Lane resolution applies it through `NextPreset`, a `devlanes-core` `Contributor`; the composition root registers it and feeds `options_schema()` to the config.

## Usage

```rust
use devlanes_preset_next::{NextOptions, contribution};

let added = contribution(20013, &NextOptions::default());
assert_eq!(added.args, ["--port", "20013", "--hostname", "127.0.0.1"]);
```

## API

- `NextPreset`: the contributor; only acts on services with `preset: next`.
- `NextOptions`: the `next:` options; unknown keys are rejected. `merge` applies overlays.
- `parse_options` and `options_schema`: evaluated JSON to options, and the JSON Schema for config.
- `contribution`: the flags for a port, without a launch draft.

## Config example

```yaml
services:
  site:
    # Adds Next's port and hostname flags to the command.
    preset: next
    run_script: apps/site/package.json#dev
    # Options for the next preset.
    next:
      # Appended after the preset's flags.
      args: [--turbopack]
```

The preset adds `--port <self.port> --hostname 127.0.0.1`; Next binds `0.0.0.0` by default. With `run_script` the script runner places the flags (after `--` for npm); a plain `run` gets them appended, with `--` first when it is `npm run ...`.

## License

Apache-2.0
