# devlanes-preset-vite

`preset: vite` binds a Vite dev server to the service's allocated port. Lane resolution applies it through `VitePreset`, a `devlanes-core` `Contributor`; the composition root registers it and feeds `options_schema()` to the config.

## Usage

```rust
use devlanes_preset_vite::{ViteOptions, contribution};

let added = contribution(20012, &ViteOptions::default());
assert_eq!(added.args, ["--port", "20012", "--strictPort", "--host", "127.0.0.1"]);
```

## API

- `VitePreset`: the contributor; only acts on services with `preset: vite`.
- `ViteOptions`: the `vite:` options; unknown keys are rejected. `merge` applies overlays.
- `parse_options` and `options_schema`: evaluated JSON to options, and the JSON Schema for config.
- `contribution`: the flags for a port, without a launch draft.

## Config example

```yaml
services:
  web:
    # Adds Vite's port and host flags to the command.
    preset: vite
    run_script:
      file: apps/web/package.json
      name: dev
    # Options for the vite preset.
    vite:
      # Appended after the preset's flags.
      args: [--mode, staging]
```

The preset adds `--port <self.port> --strictPort --host 127.0.0.1`. `--strictPort` makes Vite exit instead of moving to another port, which would break the proxy route. The script runner places the flags after the script name (after `--` for npm). The preset needs `run_script`: config validation refuses it next to `run`.

## License

Apache-2.0
