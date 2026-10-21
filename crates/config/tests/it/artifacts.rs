/// Served artifacts are the reviewed schema baseline; no parallel schema snapshots are needed.
#[test]
fn committed_schemas_are_current() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for (name, schema) in [
        ("schema.json", super::registered_schema()),
        ("user-schema.json", devlanes_config::user_schema()),
    ] {
        let generated = devlanes_config::schema_artifact(schema).expect("serialize schema");
        let committed = std::fs::read(directory.join(name))
            .unwrap_or_else(|error| panic!("{name}: {error}; run `cargo build -p devlanes`"));
        assert!(
            committed == generated.as_bytes(),
            "{name} is stale; run `cargo build -p devlanes`"
        );
    }
}
