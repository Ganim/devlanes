# devlanes-pages

The error pages `devlanes-proxy` serves when it answers a request itself: unknown hosts, stopped or failed services, upstream errors. Each page is self-contained HTML (inline CSS and logo, no requests) or the same content as Markdown, and names a stable error code documented in `docs/errors/`.

## Usage

```rust
use devlanes_pages::{ErrorCode, ErrorPage, Format};

let accept = Some("text/html,*/*;q=0.8");
let page = ErrorPage::new(ErrorCode::ServiceFailed)
    .unit(Some("acme@2"), Some("web"))
    .logs("Error: listen EADDRINUSE 127.0.0.1:20101");
let format = Format::negotiate(accept);
let body = page.render(format);
assert!(body.contains("DL203"));
```

## API

- `ErrorCode`: every error with its ID (`DL203`), slug (`service-failed`), status, title and severity.
- `ErrorPage`: builder for one response; `render` escapes everything it is given.
- `Format::negotiate`: HTML for clients that ask for `text/html`, Markdown for the rest.
- `CONTENT_SECURITY_POLICY`: allows the inline stylesheet by hash and nothing else.

## Editing the pages

The page is `templates/page.html` and `templates/page.md` (askama) with `styles/page.scss`. `build.rs` compiles the SCSS to compressed CSS, hashes it for the policy and strips every line break and indentation from the HTML template, so a line break there never stands in for a space. To look at every page with sample data:

```sh
cargo run -p devlanes-pages --example preview
open target/pages-preview/index.html
```

A new code needs a page at `docs/errors/<slug>.md` and a row in `docs/errors/README.md`; a test checks both.

## License

Apache-2.0
