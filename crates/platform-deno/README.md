# devlanes-platform-deno

Launch contributor for Deno units. The binary registers it and `devlanes-resolve` applies it to units whose platform is `deno`, set in the config or detected from a Deno config file.

## Usage

```rust
use std::sync::Arc;
use devlanes_core::contrib::Contributor;
use devlanes_platform_deno::DenoContributor;

let deno: Arc<dyn Contributor> = Arc::new(DenoContributor);
// Pass in `ResolveInput::contributors`.
```

## Config example

```yaml
services:
  api:
    cwd: apps/api                        # has deno.json: platform inferred
    run: deno task dev
  cron:
    platform: deno
    run: deno run -A cron.ts
```

Deno units get:

- `DENO_CERT`: the devlanes root certificate (the combined bundle when only that exists). Deno adds it to its root store, so the Mozilla roots stay trusted. A different value from the shell is replaced and reported as a note.
- `DENO_NO_UPDATE_CHECK=1`, so service starts do not print upgrade notices.
- `FORCE_COLOR=1` (honoured since Deno 2.2.4), unless the shell sets `FORCE_COLOR` or `NO_COLOR`.

## Detection

Without `platform`, Deno claims a unit whose `run_script` source is `deno.json` or `deno.jsonc`, or whose working directory contains one, above node's claim on a `package.json` there. A Deno config in a parent directory inside the lane gives a weaker claim that a `package.json` in the working directory beats.

## License

Apache-2.0
