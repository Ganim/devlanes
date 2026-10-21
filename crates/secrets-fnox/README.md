# devlanes-secrets-fnox

`SecretsProvider` that resolves a lane's secrets with `fnox env --json`. The binary registers it and `devlanes-resolve` calls it once per fnox profile (the project's `fnox.profile` or a service's own) for each lane resolution.

## Usage

```rust
use devlanes_core::contrib::{SecretsProvider, SecretsRequest};
use devlanes_secrets_fnox::FnoxProvider;
use std::{collections::BTreeMap, path::Path};

async fn load(client_env: &BTreeMap<String, String>) {
    let fnox = FnoxProvider::default();
    let request = SecretsRequest {
        worktree: Path::new("/work/acme"),
        profile: Some("dev"),
        explicit: true,
        base_env: client_env,
    };
    match fnox.env(&request).await {
        Ok(secrets) => println!("{} variables", secrets.set.len()),
        Err(error) => eprintln!("{error}"),
    }
}
```

## Config example

```yaml
fnox:
  profile: dev                  # project profile; default: fnox's default profile
services:
  api:
    run: pnpm dev               # gets the `dev` profile's secrets
  admin-api:
    run: pnpm -F admin dev
    fnox: { profile: admin-api }   # this service uses its own profile instead
```

Each unit gets the variables in fnox's `set`, at the secrets layer: above the client env and contributors, below `DEVLANES_*`, `global_env` and service `env`. Names in fnox's `remove` are dropped from the client environment. When a provider needs a sign-in, the start fails and asks for `devlanes unlock`, which runs fnox in the user's terminal.

## Requirements

Requires fnox 1.39 or later, the first release with `fnox env --json`. Older versions fail with an error that names the required version.

## Behaviour

- Runs `fnox [--profile P] env --json --non-interactive` in the lane worktree with the client environment, and returns `set`, `remove` and `files` from the document.
- Without an fnox config file in the worktree, it returns nothing unless the project config names fnox (`explicit`).
- A resolution error that asks for a sign-in, unlock or prompt becomes `SecretsError::Locked`; other errors are `SecretsError::Other` with fnox's message.
- Values are never logged or written to disk. `SecretEnv` has no `Debug`.
- Times out after 30 seconds by default.

`parse_document` parses a captured document without running fnox.

## License

Apache-2.0
