# devlanes-platform-rust

Launch contributor for Rust units. The binary registers it and `devlanes-resolve` applies it to units whose platform is `rust`, set in the config or detected from a `Cargo.toml`.

## Usage

```rust
use std::sync::Arc;
use devlanes_core::contrib::Contributor;
use devlanes_platform_rust::RustContributor;

let rust: Arc<dyn Contributor> = Arc::new(RustContributor);
// Pass in `ResolveInput::contributors`.
```

## Config example

```yaml
services:
  api:
    cwd: crates/api                      # has Cargo.toml: platform inferred
    run: cargo run
  worker:
    platform: rust
    run: ./target/debug/worker
```

Rust units get:

- `CARGO_TERM_COLOR=always`, unless the shell sets `CARGO_TERM_COLOR` or `NO_COLOR`.
- `SSL_CERT_FILE`: the combined bundle of system roots plus the devlanes root. OpenSSL (the `openssl` crate, `native-tls` on Linux) and `rustls-native-certs` load this file instead of the system store. A different value from the shell is replaced and reported as a note.

`RUST_LOG` is never set. `native-tls` on macOS reads the keychain, which `devlanes certs trust` covers. Programs that only use `webpki-roots` ignore every variable and do not trust the devlanes CA.

## Behaviour

Without `platform`, Rust claims a unit whose `run_script` source is `Cargo.toml`, or whose working directory contains one. The claim sits below node's, so a directory with both `Cargo.toml` and `package.json` stays node.

## License

Apache-2.0
