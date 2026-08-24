# HTTP for Zed

Language support for `.http` request files in [Zed](https://zed.dev/), based on
the JetBrains [HTTP request-in-editor
specification](https://github.com/JetBrains/http-request-in-editor-spec).

This repository is a fork of
[`tie304/zed-http`](https://github.com/tie304/zed-http). See
[Credits](#credits) for the upstream changes and later fork work incorporated
here.

## Features

- Current canonical
  [`rest-nvim/tree-sitter-http`](https://github.com/rest-nvim/tree-sitter-http)
  grammar.
- Highlighting for methods, URLs, headers, variables, declarations, metadata,
  bodies, redirects, scripts, responses, and request separators.
- Embedded JSON, XML, GraphQL, JavaScript, and comment highlighting.
- Outline entries for named, unnamed, and initial request sections.
- Language-provided runnable tasks for one request or every request in a file.
- Optional LSP-powered code lenses, code actions, hover summaries, cached
  responses, headers-only views, and saved response files.

## Install the extension

Until this fork is published in the Zed extension registry, install it as a
development extension:

1. Clone `https://github.com/raphaelluethy/zed-http`.
2. In Zed, run `zed: install dev extension`.
3. Select the cloned repository.

Zed builds the extension for `wasm32-wasip2` and fetches the pinned grammar.

## Runnable tasks

The extension ships
[`languages/http/tasks.json`](languages/http/tasks.json), so you no longer need
to copy task definitions into every project.

Runnable tasks execute in Zed's terminal and therefore require `httpyac` on your
shell `PATH`:

```bash
npm install --global httpyac
```

- Use the gutter run icon or code actions on a request method to send that
  request.
- Run `task: spawn` and select **Send all HTTP requests in file** to execute the
  file with `httpyac send --all`.
- Override the `http-request` task tag in project or global `tasks.json` if you
  prefer another CLI.

The LSP's managed `httpyac` installation is isolated from terminal tasks, which
is why the runnable path still needs a shell-visible command.

## HTTP language server

The extension provides `zed-http-lsp`, which delegates execution to `httpyac`.
This preserves httpyac behavior for variables, environment files, scripts,
assertions, and supported protocols.

For a published release, the extension:

1. Uses a configured `zed-http-lsp` binary or one already on `PATH`.
2. Otherwise downloads the matching binary from this repository's latest
   GitHub release.
3. Uses `settings.httpyac.path` when a custom `httpyac` executable is
   configured.
4. Otherwise installs the pinned `httpyac` npm package through Zed's extension
   API and invokes it with Zed's managed Node runtime.

With the default managed runtime, no global Node or `httpyac` installation is
required for the LSP path.

Enable clickable code lenses in Zed settings:

```json
{
    "code_lens": "on"
}
```

The LSP exposes:

- **Send** — execute the request containing the selected line from the current
  editor buffer, including unsaved changes.
- **Show** — reopen the last cached response.
- **Headers** — show only status and response headers.
- **Save** — write the last response beside the source file as `.http-resp`.
- Hover status and timing for the last response.
- Equivalent actions through `cmd-.` / `ctrl-.`.

### Local LSP development

Before the first tagged release, build the server locally:

```bash
cargo build --release --package zed-http-lsp
```

Then point Zed at the resulting binary:

```json
{
    "lsp": {
        "zed-http-lsp": {
            "binary": {
                "path": "/absolute/path/to/zed-http/target/release/zed-http-lsp"
            }
        }
    }
}
```

To use a custom `httpyac` executable instead of the managed npm package:

```json
{
    "lsp": {
        "zed-http-lsp": {
            "settings": {
                "httpyac": {
                    "path": "/absolute/path/to/httpyac"
                }
            }
        }
    }
}
```

## Development

The repository pins [Rust 1.98.0](https://blog.rust-lang.org/2026/08/20/Rust-1.98.0/)
in [`rust-toolchain.toml`](rust-toolchain.toml), including the `wasm32-wasip2`
target used by Zed extensions. rustup installs that toolchain automatically in
this directory.

```bash
cargo fmt --all --check
cargo test --workspace --all-targets
cargo build --target wasm32-wasip2
```

The workspace sets `default-members = ["."]` because Zed runs
`cargo build --target wasm32-wasip2` from the repository root. Native LSP code
must not be cross-compiled into the extension component.

Pushing a `v*` tag runs
[`.github/workflows/release.yml`](.github/workflows/release.yml), verifies the
workspace, and publishes native LSP archives for ARM64 and x86-64 macOS, Linux,
and Windows.

## Credits

- Original extension:
  [`tie304/zed-http`](https://github.com/tie304/zed-http).
- Body-language injection work:
  [`tie304/zed-http#1`](https://github.com/tie304/zed-http/pull/1) and
  [`tie304/zed-http#6`](https://github.com/tie304/zed-http/pull/6).
- Outline and variable-highlighting proposals:
  [`tie304/zed-http#3`](https://github.com/tie304/zed-http/pull/3) and
  [`tie304/zed-http#9`](https://github.com/tie304/zed-http/pull/9).
- Initial native LSP proposal:
  [`tie304/zed-http#7`](https://github.com/tie304/zed-http/pull/7).
- The httpyac-backed code-lens and response-display design was informed by the
  later [`ToyVo/zed-http`](https://github.com/ToyVo/zed-http) fork.
