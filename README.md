> [!WARNING]
> **Experimental and a heavy work in progress.** Compatibility, installation behavior, and cache formats may change. Do not rely on this extension for production-critical workflows yet.

# HTTP extension for Zed

`zed-http` lets projects share JetBrains-style `.http` files between IntelliJ-based IDEs and Zed. Kulala LS provides language intelligence; this project's own adapter parses and sends the requests. It is not affiliated with JetBrains or the Kulala project.

## Architecture

Zed runs two language servers for `.http` files:

- **Kulala LS** (`@mistweaverco/kulala-ls@1.11.1`) is the HTTP language server and owns completion and hover.
- **The execution adapter** (`zed-http-lsp`) is a single native binary that owns code lenses, commands, response buffers and request execution. Zed currently exposes these editor actions to extensions through a language-server slot, so the adapter speaks the narrow LSP subset required for them.

The adapter parses `.http` files, resolves variables, runs scripts and sends requests in process. It needs no sidecar, Node.js or other runtime.

```diagram
┌─────┐     ┌───────────┐
│ Zed │────▶│ Kulala LS │  completion and hover
└──┬──┘     └───────────┘
   │
   ▼
┌──────────────────────────────────────────┐
│ zed-http-lsp                             │
│ parser · variables · scripts (Boa) ·     │
│ HTTP / GraphQL / WebSocket / gRPC        │
└──────────────────────────────────────────┘
```

Inside the adapter:

- `syntax.rs` parses the IntelliJ HTTP client format.
- `variables.rs` resolves environments, file variables, dynamic variables and named-response references.
- `script.rs` runs pre-request scripts and response handlers on the embedded [Boa](https://boajs.dev) JavaScript engine.
- `protocol/` sends HTTP requests (reqwest over rustls), GraphQL, WebSocket and gRPC.

## Features

- Syntax highlighting, completion and hover for HTTP files
- **▶ Send** and **▶ Send All** code lenses that use the current in-memory document, including unsaved edits
- Response buffers with full, headers-only, reopen and save actions
- IntelliJ-style requests:
  - `###` separators, `# @name`, request lines with an optional HTTP version, multi-line URLs and headers
  - `@no-redirect`, `@no-cookie-jar`, `@no-log`, `@timeout` and `@connection-timeout`. For HTTP and GraphQL, `@timeout` is an inactivity timeout: it ends a request that receives nothing for that long, so slow streams keep going, and the whole request is still capped at 5 minutes or the timeout, whichever is longer. For WebSocket and gRPC it bounds the whole exchange. Durations are seconds, or take an `ms`, `s` or `m` suffix.
  - inline bodies, `< file` and `<@ file` includes, and multipart bodies
- Variables:
  - `http-client.env.json` and `http-client.private.env.json`, including `$shared`
  - file variables (`@name = value`)
  - dynamic variables: `$uuid`, `$timestamp`, `$isoTimestamp`, `$random.*`, `$env.NAME`, `$processEnv NAME`
  - named-response references such as `{{login.response.body.$.token}}` and `{{login.response.headers.X-Token}}`
- Pre-request scripts (`< {% %}` or `< file.js`) and response handlers (`> {% %}` or `> file.js`) with the IntelliJ `client`, `request` and `response` objects, plus `jsonPath`, `crypto` digests and `$random`
- Response redirects (`>> file` and `>>! file`), `import` and `run`
- `client.global` values and cookies shared by all requests while the adapter runs

### GraphQL

`GRAPHQL url` sends the body as a JSON `POST`. An optional JSON variables object can follow the query. `operationName` comes from the first named operation; fragment definitions are skipped.

### WebSocket

`WEBSOCKET ws://…` (or `wss://`) opens a connection with the request headers on the handshake. The body is split into messages on `===` lines, and `=== wait-for-server` waits for one server message before the next is sent. After the last message, server messages are collected until the connection has been idle for 2 seconds, with a 30 second cap on the whole exchange. An explicit `@timeout` replaces both: the exchange ends after that long, or after that long without a server message. The exchange renders as `→` / `←` lines.

### gRPC

`GRPC host:port/package.Service/Method` sends the JSON body as the request message; `grpcs://` or `https://` uses TLS, otherwise plaintext. Request headers become call metadata (`-bin` headers take base64 values). Method descriptors come from server reflection (v1, then v1alpha); without reflection, `.proto` files from the request file's directory up to the workspace root are compiled with protox. Unary and server-streaming methods are supported. Each response message renders as pretty JSON, followed by the gRPC status.

### Known gaps compared to IntelliJ

- Globals and cookies are kept in memory and are lost when Zed restarts the adapter.
- There is no request history. `@no-log` only keeps the response body out of response buffers.
- Scripts have no `require`, timers, file system or network access, and cannot read environment variables. They run with a 10 second time limit.
- OAuth 2.0 (`$auth.token`), client certificates and proxy settings from env files, HTTP/3 and the IntelliJ example server are not supported.
- GraphQL `operationName` is taken from the first named operation in the query and cannot be chosen explicitly.
- gRPC client-streaming and bidirectional-streaming methods are not supported, and descriptors (reflection or `.proto` compilation) are resolved again for every request.
- gRPC `google.protobuf.Any` payloads only resolve when their type is in a file the service's descriptors already include.
- The cookie jar, `@no-cookie-jar` and `@no-redirect` apply to HTTP and GraphQL only: WebSocket handshakes and gRPC calls neither send nor store session cookies. `@no-log` applies to every protocol.

## Installation and use

Install a development checkout with **zed: install dev extension** and select this repository. When an HTTP file is first opened, Zed installs the pinned Kulala LS package and downloads this project's platform archive, which contains only `zed-http-lsp`. If the archive cannot be downloaded, for example while offline, the extension reuses a previously downloaded adapter. Without one, request execution is unavailable but completion and hover keep working.

Use **▶ Send** above a request or `run` line, or **▶ Send All** above the first one. Requests run in the background and show progress in the status bar. After execution, the adapter refreshes the code lenses and caches the response for **Show**, **Headers** and **Save**. Click **Show** or **Headers** to open the response; Zed does not currently let a generic extension language-server process force-open a response tab.

Nothing is installed into the project or as a global package. Request execution does not require Node.js, npm or a task shell on `PATH`; Kulala LS runs with Zed's managed Node.js runtime.

## Environments and state

The adapter looks for `http-client.env.json` and `http-client.private.env.json` from the request file's directory up to the workspace root, and uses the nearest copy of each. Files outside the workspace only use env files next to them. It selects:

1. `ZED_HTTP_ENV`, when configured;
2. `default`, when present;
3. otherwise the alphabetically first environment. `$shared` holds variables shared by all environments and is never selected.

Variables resolve in this order, highest first: request variables set by a pre-request script, `client.global`, file variables, the private environment, the public environment, private `$shared`, then public `$shared`. An unresolved `{{name}}` is sent verbatim and reported as a warning in the response.

Keep secrets in `http-client.private.env.json` and exclude it from version control.

`client.global` values, cookies and named responses live in the adapter's memory only. They are shared by every file in the Zed session and disappear when the adapter exits.

## Security model

Only execute `.http` files you trust. Requests can read files through body includes and write files through response redirects, with your permissions. `$env` variables in requests can read the worktree environment that the adapter inherits.

The adapter adds the following boundaries:

- Scripts run in a separate worker process (the same binary) on the embedded Boa engine. They have no file system, network or environment access, are limited in loop iterations and recursion, and are killed after 10 seconds. At most two run at once.
- Variable expansion is bounded in depth and output size, request and response bodies are capped at 32 MiB, and HTTPS uses rustls with no OpenSSL dependency.
- Temporary response files use a process-private directory and mode `0600` on Unix, and are removed when the adapter shuts down. Explicitly saved responses also use mode `0600` on Unix.
- The Kulala LS version is pinned. Release CI actions are commit-pinned, write permission is limited to the publish job, and release archives include `SHA256SUMS`.

Response buffers and saved files can contain authorization headers, cookies, tokens and private response data. Review them before sharing.

The extension capability manifest restricts downloads to this project's GitHub releases and npm installation to `@mistweaverco/kulala-ls`.

## Supported platforms

Release bundles are built for:

- macOS arm64 and x86-64
- Linux arm64 and x86-64
- Windows arm64 and x86-64

## Development

Build the adapter:

```bash
cargo build --package zed-http-lsp
```

For a local Zed build, point the extension at it:

```json
{
  "lsp": {
    "zed-http-lsp": {
      "binary": {
        "path": "/absolute/path/to/zed-http/target/debug/zed-http-lsp"
      }
    }
  }
}
```

Verify the workspace with the following commands. The integration tests start in-process servers, so they need no network access.

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo build --locked --target wasm32-wasip2
cargo build --locked --package zed-http-lsp
node scripts/smoke_lsp.cjs
```
