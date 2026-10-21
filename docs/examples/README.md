# Configuration examples

These sanitized examples preserve the source configuration forms with generic project names. They describe the v1 contract; companion application files and infrastructure are not included. Native runnable fixture services are under `fixtures/`.

## Usage

- `minimal.yaml`: shared Postgres, per-lane database, API and web app.
- `devlanes.yaml`: reference forms, templates, tasks, service presets and profiles, with a comment on each section.
- `devlanes.local.yaml`: merge example. Its `admin-web` entry has no matching service in the reference base and lacks a launch command. Use a base defining that service before testing this override; do not treat the supplied pair as valid.
- `user-config.yaml`: per-user settings. The source includes `ios-simulator` and `android-emulator` trust targets, which v1 rejects. They remain explicit here to document the discrepancy. Remove those targets for a v1 configuration. Public DNS resolution and login/relay account setup are also outside v1.

`devlanes-config` tests parse every file here, so the examples stay valid.

## Behaviour

Map keys merge; lists and scalars replace. Merge precedes template expansion. A `.local.yaml` file is normally gitignored; this documentation copy is intentionally tracked.

## License

Apache-2.0
