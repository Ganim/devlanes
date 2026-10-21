/// Re-embeds migrations whenever their SQL or metadata changes.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
