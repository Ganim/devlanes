# devlanes-preset-vite

`preset: vite` binds a Vite dev server to the service's allocated port. Lane resolution applies it through `VitePreset`, a `devlanes-core` `Contributor`; the composition root registers it and feeds `options_schema()` to the config.

## Usage

```yaml
services:
  web:
    preset: vite
    run_script: apps/web/package.json#dev
    vite:
      args: [--mode, staging]   # optional, appended after the preset's flags
```

The preset adds `--port <self.port> --strictPort --host 127.0.0.1`. `--strictPort` makes Vite exit instead of moving to another port, which would break the proxy route. With `run_script` the script runner places the flags (after `--` for npm); a plain `run` gets them appended, with `--` first when it is `npm run ...`.

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

## License

Apache-2.0
