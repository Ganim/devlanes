//! Writes every error page, as HTML and Markdown, with sample data:
//! `cargo run -p devlanes-pages --example preview -- <dir>` (default `target/pages-preview`),
//! then open `<dir>/index.html`.

use std::{fs, io, path::PathBuf};

use devlanes_pages::{ErrorCode, ErrorPage, Format, KnownHost};

fn main() -> io::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("target/pages-preview"), PathBuf::from);
    fs::create_dir_all(&dir)?;
    let known = vec![
        KnownHost {
            host: "acme.localhost",
            owner: Some("acme@0"),
        },
        KnownHost {
            host: "api.acme.localhost",
            owner: Some("acme@0"),
        },
        KnownHost {
            host: "acme-1.localhost",
            owner: Some("acme@1"),
        },
        KnownHost {
            host: "api.acme-1.localhost",
            owner: Some("acme@1"),
        },
        KnownHost {
            host: "*.shop.localhost",
            owner: Some("shop@0"),
        },
    ];
    let logs = "> acme@0.1.0 dev\n> vite --port 20101\n\nError: listen EADDRINUSE: address already in use 127.0.0.1:20101\n    at Server.setupListenHandle (node:net:1908:16)";
    let mut index =
        String::from("<!doctype html><meta charset=utf-8><title>devlanes pages</title><ul>");
    for code in ErrorCode::ALL {
        let page = ErrorPage::new(code).host("acme-2.localhost");
        let page = match code {
            ErrorCode::UnknownHost => page.known_hosts(known.clone()),
            ErrorCode::ServiceIdle => page
                .unit(Some("acme@2"), Some("web"))
                .hint("devlanes start --lane acme@2"),
            ErrorCode::ServiceFailed => page
                .unit(Some("acme@2"), Some("web"))
                .hint("devlanes start --lane acme@2")
                .logs(logs),
            ErrorCode::BoxReconnecting => page
                .unit(Some("acme@2"), Some("web"))
                .hint("Start it on my-box: devlanes start --lane acme@2"),
            _ => page.unit(Some("acme@2"), Some("web")),
        };
        fs::write(dir.join(format!("{code}.html")), page.render(Format::Html))?;
        fs::write(
            dir.join(format!("{code}.md")),
            page.render(Format::Markdown),
        )?;
        index.push_str(&format!(
            "<li>{} <a href=\"{code}.html\">{code}</a> (<a href=\"{code}.md\">md</a>)</li>",
            code.id()
        ));
    }
    index.push_str("</ul>");
    fs::write(dir.join("index.html"), index)?;
    println!("{}", dir.join("index.html").display());
    Ok(())
}
