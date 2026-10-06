> [!WARNING]
> **Experimental and a heavy work in progress.** Compatibility, installation behavior, and cache formats may change. Do not rely on this extension for production-critical workflows yet.

# HTTP extension for Zed

`zed-http` lets projects share JetBrains-style `.http` files between IntelliJ-based IDEs and Zed. It uses Kulala for both language intelligence and request semantics; this project only supplies the Zed integration. It is not affiliated with JetBrains or the Kulala project.

## Architecture

Zed runs Kulala LS plus a separate request-execution path:

- **Kulala LS** (`@mistweaverco/kulala-ls@1.11.1`) is the HTTP language server and owns completion and hover.
- **The execution adapter** (`zed-http-lsp`) owns only Zed code lenses, commands, response buffers, and process supervision. Zed currently exposes these editor actions to extensions through a language-server slot, so the adapter speaks the narrow LSP subset required for those actions. It provides no HTTP language intelligence and does not parse or execute requests.

For every index or execution operation, the adapter starts the bundled **Kulala Core 0.36.0** sidecar, sends one JSON request, reads one JSON response, and lets the process exit.

```diagram
┌─────┐     ┌───────────┐
│ Zed │────▶│ Kulala LS │  completion and hover
└──┬──┘     └───────────┘
   │
   ▼
┌───────────────────┐     ┌─────────────┐
│ Execution adapter │────▶│ Kulala Core │  parse, run, scripts, state
│ (editor bridge)   │     │ sidecar     │
└───────────────────┘     └─────────────┘
```

There is no custom Rust HTTP parser, client, JavaScript engine, cookie jar, or standalone `zed-http-runner` CLI. Keeping Kulala Core as the single execution boundary avoids the syntax drift that occurred when those features were reimplemented locally.

## Features

- Syntax highlighting, completion, and hover for HTTP files
- **▶ Send** and **▶ Send All** code lenses using the current in-memory document, including unsaved edits
- Response buffers with full, headers-only, reopen, and save actions
- Kulala Core support for IntelliJ-style variables and environments, external bodies, multipart requests, pre-request and response scripts, request chaining, GraphQL, WebSocket, gRPC, imports, and response redirection
- Persistent `client.global` values and cookies using Kulala Core's state store

Kulala Core is the compatibility boundary. The adapter passes the complete source document to Core and does not filter syntax. This provides substantially broader IntelliJ HTTP compatibility than the removed local parser, but it does not imply exact JetBrains IDE parity. In particular, compatibility depends on the pinned Core release; custom methods, executable HTTP/3, and some IDE-specific behaviors remain upstream gaps.

## Installation and use

Install a development checkout with **zed: install dev extension** and select this repository. When an HTTP file is first opened, Zed installs the pinned Kulala LS package and downloads this project's platform archive. That archive contains both the execution adapter and Kulala Core. If the archive cannot be downloaded, for example while offline, the extension reuses a previously downloaded adapter; without one, only request execution is unavailable and completion and hover keep working.

Use **▶ Send** above one request or **▶ Send All** above the first request. Requests run in the background and show progress in the status bar. After execution, the adapter refreshes the code lenses and caches the response for **Show**, **Headers**, and **Save**. Click **Show** or **Headers** to open it; Zed does not currently let a generic extension language-server process force-open a response tab.

Nothing is installed into the project or as a global package. Request execution does not require Node.js, npm, or a task shell on `PATH`; Kulala LS runs with Zed's managed Node.js runtime.

## Environments and state

Kulala Core discovers standard `http-client.env.json` and `http-client.private.env.json` files. The adapter selects:

1. `ZED_HTTP_ENV`, when configured;
2. `default`, when present;
3. otherwise the alphabetically first environment. `$shared` holds variables shared by all environments and is never selected.

Keep secrets in `http-client.private.env.json` and exclude it from version control.

Kulala Core persists globals and cookies in its OS application-data `kulala.db`. Core 0.36.0 does not expose a portable per-workspace storage override, so this state is shared between projects for the same OS user. Use distinct global names where cross-project collisions matter and clear sensitive Core state when it is no longer needed.

## Security model

Only execute `.http` files you trust. IntelliJ-style scripts, environment access, external body files, and external tools are intentionally powerful and run with the user's permissions. The adapter inherits the worktree environment so `$env` syntax remains compatible; request scripts can therefore read those environment variables.

The integration adds the following boundaries:

- Kulala LS and Core versions are pinned. Every Core platform asset is checked against a committed SHA-256 digest before release packaging.
- Release CI actions are commit-pinned, write permission is limited to the publish job, and release archives include `SHA256SUMS`.
- The adapter resolves Core only from explicit `KULALA_CORE_PATH` or beside its own executable; it never auto-runs a `kulala-core` found on `PATH`.
- Each Core operation has a hard timeout. Input is limited to 16 MiB, stdout to 32 MiB, and diagnostics to 1 MiB. Dropping or cancelling an operation kills its child process.
- Temporary response files use a process-private directory and mode `0600` on Unix, then are removed when the adapter shuts down. Explicitly saved responses also use mode `0600` on Unix.

Response buffers and saved files can contain authorization headers, cookies, tokens, and private response data. Review them before sharing.

The extension capability manifest restricts downloads to this project's GitHub releases and npm installation to `@mistweaverco/kulala-ls`.

## Supported platforms

Release bundles are built for:

- macOS arm64 and x86-64
- Linux arm64 and x86-64
- Windows x86-64

Kulala Core 0.36.0 does not publish a Windows arm64 executable, so that platform is intentionally rejected instead of publishing a nonfunctional adapter.

## Development

Fetch and verify the pinned Core binary, then build the adapter:

```bash
node scripts/fetch_kulala_core.cjs
cargo build --package zed-http-lsp
```

For a local Zed build, configure both executables explicitly:

```json
{
  "lsp": {
    "zed-http-lsp": {
      "binary": {
        "path": "/absolute/path/to/zed-http/target/debug/zed-http-lsp",
        "env": {
          "KULALA_CORE_PATH": "/absolute/path/to/zed-http/target/kulala-core/kulala-core"
        }
      }
    }
  }
}
```

Verify the workspace with:

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo build --locked --target wasm32-wasip2
cargo build --locked --package zed-http-lsp
node scripts/smoke_lsp.cjs
```
