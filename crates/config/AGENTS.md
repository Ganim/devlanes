# Config schema documentation

Every config field, struct, enum, and variant must have a doc comment that reads well as an editor hover and a schemars example. Explain its purpose, default, and any useful constraint in plain words; avoid Rust type names in hover text.

A hover shows the field's own doc comment, so it must stand alone: what the setting does, why you would set it, the default, and what happens at the edges (a timeout, a failure). `schema_docs.rs` drops the descriptions of untagged-union variants from the schema, so a field that takes several forms (one entry or a list, `true` or a port name) names them itself. Unit-enum variant docs become the editor's list of allowed values; each says what choosing that value does. The schema has no titles, the root's included, because yaml-language-server prints one as a heading on every hover.

Keep descriptions and examples beside the Rust declarations and consistent with `docs/examples`. Preset option declarations follow the same rule. The schema-walk test checks descriptions and examples on properties and definitions, validates the examples, and rejects any title, the root's included.

Regenerate `schema.json` and `user-schema.json` with `cargo build -p devlanes` and include the generated files in the commit. Do not edit JSON by hand. The artifact guard checks that the files match the Rust declarations.
