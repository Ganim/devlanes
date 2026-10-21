# devlanes-preset-wrangler

`preset: wrangler` gives each lane's `wrangler dev` its own HTTP port, inspector port, vars and dev registry. Lane resolution applies it through `WranglerPreset`, a `devlanes-core` `Contributor` that also asks for the named port `inspector`; the composition root registers it and feeds `options_schema()` to the config.

## Usage

```yaml
services:
  api:
    preset: wrangler
    run_script: apps/api/package.json#dev
    wrangler:
      vars:
        PUBLIC_BASE_URL: ${{ self.public_url }}
      args: [--env-file, .dev.vars.local]
      # isolate_registry: false    # default: true
```

The preset adds `--port <self.port> --inspector-port <self.ports.inspector>`, then `--var KEY:VALUE` for each var in key order, then `args`. Vars go on the command line because `vars` in the Wrangler config override the process environment, while `--var` overrides the config. It also sets `WRANGLER_REGISTRY_PATH=<lane>/.devlanes/wrangler-registry`, so same-named workers in two lanes do not bind to each other. That variable is not in Wrangler's user docs; it is defined in cloudflare/workers-sdk.

```rust
use std::collections::BTreeMap;
use std::path::Path;
use devlanes_preset_wrangler::{WranglerInput, WranglerOptions, contribution};

let ports = BTreeMap::from([("inspector".to_owned(), 20011)]);
let added = contribution(&WranglerInput {
    port: 20010,
    ports: &ports,
    lane_worktree: Path::new("/lanes/acme-1"),
    options: &WranglerOptions::default(),
    script_body: Some("wrangler dev --persist-to .state"),
})?;
assert_eq!(added.args, ["--port", "20010", "--inspector-port", "20011"]);
```

## API

- `WranglerPreset`: the contributor; only acts on services with `preset: wrangler`.
- `WranglerOptions`: the `wrangler:` options; var values may be strings, numbers or booleans.
- `parse_options` and `options_schema`: evaluated JSON to options, and the JSON Schema for config.
- `contribution` and `check_script`: the flags and env without a launch draft, and the script check on its own.

## Hardcoded flags

With `run_script`, the script body is checked for `--port`, `--inspector-port` and `--inspectorPort` (with a value or `=value`). Any of them fails the launch, because the preset would pass the flag a second time. The error names the flag and the fix: remove it from the script, or drop the preset and pass `--port ${{ self.port }} --inspector-port ${{ self.ports.inspector }}` in `run`. Plain `run` commands are not checked.

## License

Apache-2.0
