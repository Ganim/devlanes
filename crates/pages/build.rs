//! Compiles `styles/page.scss` to compressed CSS and hashes it for the pages'
//! Content-Security-Policy, so the stylesheet can be inlined without `'unsafe-inline'`.
//! Minifies `templates/page.html` and derives the askama template from the result.

use std::{env, fs, path::Path};

use base64::Engine;
use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-changed=styles");
    println!("cargo:rerun-if-changed=templates/page.html");
    let out = Path::new(&env::var_os("OUT_DIR").expect("set by cargo")).to_path_buf();

    let source = Path::new("styles/page.scss");
    let options = grass::Options::default().style(grass::OutputStyle::Compressed);
    let css = grass::from_path(source, &options)
        .unwrap_or_else(|error| panic!("cannot compile {}: {error}", source.display()));
    let css = css.trim_end();
    write(&out.join("page.css"), css);
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(css.as_bytes()));
    println!("cargo:rustc-env=DEVLANES_PAGES_STYLE_SHA256={hash}");

    let template = Path::new("templates/page.html");
    let html = fs::read_to_string(template)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", template.display()));
    // askama reads `source` as a Rust string literal, which `{:?}` produces.
    let derive = format!(
        "#[derive(Template)]\n#[template(source = {:?}, ext = \"html\")]\nstruct HtmlPage<'a> {{\n    v: &'a View<'a>,\n    style: &'static str,\n}}\n",
        minify(&html)
    );
    write(&out.join("html_page.rs"), &derive);
}

/// Drops every whitespace run that contains a line break and keeps the others, so the
/// template's indentation and line breaks disappear while spaces inside a line stay.
/// Rendered values are untouched, so logs keep their layout inside `<pre>`.
fn minify(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut run = String::new();
    for c in template.chars() {
        if c.is_whitespace() {
            run.push(c);
            continue;
        }
        if !run.contains('\n') {
            out.push_str(&run);
        }
        run.clear();
        out.push(c);
    }
    out
}

/// Writes a generated file, failing the build on error.
fn write(path: &Path, contents: &str) {
    fs::write(path, contents)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}
