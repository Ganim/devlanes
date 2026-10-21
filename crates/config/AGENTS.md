# Config schema documentation

Every config field, struct, enum, and variant must have a doc comment that reads well as an editor hover and a schemars example. Explain its purpose, default, and any useful constraint in plain words; avoid Rust type names in hover text.

Keep descriptions and examples beside the Rust declarations and consistent with `docs/examples`. Preset option declarations follow the same rule. The schema-walk test checks descriptions and examples on properties and definitions, validates the examples, and rejects titles below the root.

Regenerate `schema.json` and `user-schema.json` with `cargo build -p devlanes` and include the generated files in the commit. Do not edit JSON by hand. The artifact guard checks that the files match the Rust declarations.
