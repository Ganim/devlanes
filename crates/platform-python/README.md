# devlanes-platform-python

Launch contributor for Python units. The binary registers it and `devlanes-resolve` applies it to units whose platform is `python`.

## Usage

```rust
use std::sync::Arc;
use devlanes_core::contrib::Contributor;
use devlanes_platform_python::PythonContributor;

let python: Arc<dyn Contributor> = Arc::new(PythonContributor);
// Pass in `ResolveInput::contributors`.
```

## Config example

```yaml
services:
  ai-gateway:
    cwd: services/ai-gateway             # has pyproject.toml: platform inferred
    run: uv run uvicorn app:main --port ${{ self.port }}
  jobs:
    platform: python
    run: python -m jobs
```

Python units get:

- `PYTHONUNBUFFERED=1`: Python buffers stdout on pipes, so `pty: false` logs would otherwise arrive late.
- `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE`: the combined bundle of system roots plus the devlanes root. `ssl`, `requests` and `httpx` replace their roots with this file, so it must include the system roots.

## Behaviour

Without `platform`, a unit is claimed when its `run_script` source is `pyproject.toml` or `requirements.txt`, or its working directory contains either file. A directory with both a `package.json` and a Python manifest stays node.

## License

Apache-2.0
