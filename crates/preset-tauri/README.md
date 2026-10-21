# devlanes-preset-tauri

`preset: tauri` runs several Tauri v2 dev apps side by side on one machine, one per lane and replica, each with its own bundle identifier and dev URL. Lane resolution applies it through `TauriPreset`, a `devlanes-core` `Contributor`; the composition root registers it and feeds `options_schema()` to the config. The app runs where the lane runs, so the preset is for local lanes only.

## Usage

```yaml
services:
  desktop:
    preset: tauri
    run: pnpm --filter @acme/desktop dev
    replicas: 2
    tauri:
      identifier: com.acme.app.dev${{ '' if lane.index == 0 else '.lane' ~ lane.index }}.client${{ replica.index }}
      dev_url: ${{ svc.desktop_web.url }}
      before_dev_command: ""      # the frontend runs as its own service
      # product_name: Acme Dev
```

`identifier` is required. It keys the app data, webview data and keychain entries, so it must differ per lane and replica; replicas of one lane that share it fail to start. The preset adds `--config <overlay>` and sets `DEVLANES_TAURI_CONFIG` to the same path, for wrapper scripts that start `tauri dev` themselves and do not forward arguments.

```rust
use std::path::Path;
use devlanes_preset_tauri::{TauriInput, TauriOptions, contribution, overlay_path};

let options = TauriOptions {
    identifier: Some("com.acme.app.dev.lane1.client1".to_owned()),
    dev_url: Some("http://127.0.0.1:20014".to_owned()),
    ..TauriOptions::default()
};
let path = overlay_path(Path::new("/lanes/acme-1"), "desktop", 1);
let launch = contribution(&TauriInput { options: &options, overlay_path: &path })?;
// launch.overlay: {"identifier": "...", "build": {"devUrl": "http://127.0.0.1:20014"}}
```

## API

- `TauriPreset`: the contributor; only acts on services with `preset: tauri`.
- `TauriOptions`, `parse_options`, `options_schema`: the `tauri:` options, unknown keys rejected.
- `overlay` and `contribution`: the config overlay and launch additions without a draft.
- `ensure_distinct`: rejects identifiers shared across lanes, which a single lane's resolution cannot see.
- `write_overlay`: atomic write for callers outside lane resolution.

## Overlay file

Each replica's overlay goes to `<lane>/.devlanes/tauri/<service>.<replica>.conf.json`. The contributor returns it as a prepared file and the daemon writes it before spawning. It sets the top-level `identifier` and `productName` plus `build.devUrl` and `build.beforeDevCommand`, which Tauri merges over `tauri.conf.json`.

## License

Apache-2.0
