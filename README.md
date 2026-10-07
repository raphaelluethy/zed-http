> [!WARNING]
> **Experimental and a heavy work in progress.** Compatibility, installation behavior, and cache formats may change. Do not rely on this extension for production-critical workflows yet.

# HTTP extension for Zed

`zed-http` lets projects share JetBrains-style `.http` files between IntelliJ-based IDEs and Zed. A single native binary, `zed-http-lsp`, provides completion and hover and sends the requests, printing responses to Zed's terminal. It is not affiliated with JetBrains.

## Architecture

`zed-http-lsp` is a single native binary with two roles:

- **Language server.** Zed starts it for `.http` files. It provides completion, hover and parse diagnostics, and keeps the session (`client.global` values, cookies and named responses) in memory.
- **Task runner.** The gutter run arrow starts it as a Zed task with `--run`. The task forwards the request over a private local socket to the language server of its workspace, which sends it with the shared session and returns the rendered response to the terminal.

It parses `.http` files, resolves variables, runs scripts and sends requests in process. It needs no sidecar, Node.js or other runtime.

```diagram
┌─────┐  completion, hover   ┌──────────────────────────────────────────┐
│ Zed │─────────────────────▶│ zed-http-lsp (language server)           │
└──┬──┘                      │ session · parser · variables · scripts · │
   │ ▶ gutter task           │ HTTP / GraphQL / WebSocket / gRPC        │
   ▼                         └──────────────────────────────────────────┘
┌──────────────────────┐   local socket    ▲
│ zed-http-lsp --run   │───────────────────┘
│ prints to terminal   │
└──────────────────────┘
```

Inside the binary:

- `syntax.rs` parses the IntelliJ HTTP client format.
- `assist.rs` provides completion, hover and diagnostics.
- `task.rs` forwards terminal runs to the language server.
- `variables.rs` resolves environments, file variables, dynamic variables and named-response references.
- `script.rs` runs pre-request scripts and response handlers on the embedded [Boa](https://boajs.dev) JavaScript engine.
- `protocol/` sends HTTP requests (reqwest over rustls), GraphQL, WebSocket and gRPC.

## Features

- Syntax highlighting and parse diagnostics for HTTP files
- Completion for methods, headers and common header values, `# @` directives, `{{variables}}` (env files, file variables, `client.global` values, names set by scripts, named responses) and dynamic variables
- Hover on `{{variable}}` showing its value, where it comes from and what it overrides
- A gutter run arrow on every request that prints the response in Zed's terminal, with JSON pretty-printed and colored
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
- `client.global` values, cookies and named responses shared by every run in the workspace until Zed restarts

### GraphQL

`GRAPHQL url` sends the body as a JSON `POST`. An optional JSON variables object can follow the query. `operationName` comes from the first named operation; fragment definitions are skipped.

### WebSocket

`WEBSOCKET ws://…` (or `wss://`) opens a connection with the request headers on the handshake. The body is split into messages on `===` lines, and `=== wait-for-server` waits for one server message before the next is sent. After the last message, server messages are collected until the connection has been idle for 2 seconds, with a 30 second cap on the whole exchange. An explicit `@timeout` replaces both: the exchange ends after that long, or after that long without a server message. The exchange renders as `→` / `←` lines.

### gRPC

`GRPC host:port/package.Service/Method` sends the JSON body as the request message; `grpcs://` or `https://` uses TLS, otherwise plaintext. Request headers become call metadata (`-bin` headers take base64 values). Method descriptors come from server reflection (v1, then v1alpha); without reflection, `.proto` files from the request file's directory up to the workspace root are compiled with protox. Unary and server-streaming methods are supported. Each response message renders as pretty JSON, followed by the gRPC status.

### Known gaps compared to IntelliJ

- Globals, cookies and named responses are kept in memory, like session cookies, and are lost when Zed or the language server restarts. Restart the language server to start a fresh session.
- There is no request history. `@no-log` only keeps the response body out of the terminal output.
- Scripts have no `require`, timers, file system or network access, and cannot read environment variables. They run with a 10 second time limit.
- OAuth 2.0 (`$auth.token`), client certificates and proxy settings from env files, HTTP/3 and the IntelliJ example server are not supported.
- GraphQL `operationName` is taken from the first named operation in the query and cannot be chosen explicitly.
- gRPC client-streaming and bidirectional-streaming methods are not supported, and descriptors (reflection or `.proto` compilation) are resolved again for every request.
- gRPC `google.protobuf.Any` payloads only resolve when their type is in a file the service's descriptors already include.
- The cookie jar, `@no-cookie-jar` and `@no-redirect` apply to HTTP and GraphQL only: WebSocket handshakes and gRPC calls neither send nor store session cookies. `@no-log` applies to every protocol.

## Installation and use

To install a development checkout, first set it up once: run **task: spawn → zed-http: Set up development** in this project, or `rustup toolchain install && cargo setup` in a terminal. This installs the pinned Rust toolchain with the `wasm32-wasip2` target that Zed compiles the extension with, and builds `target/debug/zed-http-lsp`, which the gutter task of a dev install runs. Zed requires Rust to be installed through [rustup](https://rustup.rs) for dev extensions. Then run **zed: install dev extension** and select this repository. When an HTTP file is first opened, the extension downloads this project's platform archive, which contains only `zed-http-lsp`. If the archive cannot be downloaded, for example while offline, the extension reuses a previously downloaded binary.

Click the run arrow in the gutter next to a request to send it. Zed saves the file, and the response appears in the terminal panel with its status, timing, headers, body and test results. **HTTP: Send all requests** is available through **task: spawn**. Runs share one session per workspace: a token a login handler stores with `client.global.set` is available to the next request you run. On Windows, and whenever the language server is not running, a run starts with a fresh session.

Nothing is installed into the project or as a global package, and nothing needs Node.js, npm or a task shell on `PATH`.

## Environments and state

The runner looks for `http-client.env.json` and `http-client.private.env.json` from the request file's directory up to the workspace root, and uses the nearest copy of each. Files outside the workspace only use env files next to them. It selects:

1. `ZED_HTTP_ENV`, when configured;
2. `default`, when present;
3. otherwise the alphabetically first environment. `$shared` holds variables shared by all environments and is never selected.

Variables resolve in this order, highest first: request variables set by a pre-request script, `client.global`, file variables, the private environment, the public environment, private `$shared`, then public `$shared`. An unresolved `{{name}}` is sent verbatim and reported as a warning in the response.

Keep secrets in `http-client.private.env.json` and exclude it from version control.

`client.global` values, cookies and named responses live in the language server's memory only. They are shared by every file and run in the workspace and disappear when Zed or the language server restarts. Completion and hover read the same values.

## Security model

Only execute `.http` files you trust. Requests can read files through body includes and write files through response redirects, with your permissions. `$env` variables in requests can read the worktree environment that the language server inherits.

`zed-http-lsp` adds the following boundaries:

- Scripts run in a separate worker process (the same binary) on the embedded Boa engine. They have no file system, network or environment access, are limited in loop iterations and recursion, and are killed after 10 seconds. At most two run at once.
- Variable expansion is bounded in depth and output size, request and response bodies are capped at 32 MiB, and HTTPS uses rustls with no OpenSSL dependency.
- Terminal runs reach the language server through a Unix socket in a directory only your user can enter (`$XDG_RUNTIME_DIR` or the temp directory, mode `0700`). The socket is removed when the language server shuts down, and the session is never written to disk.
- Release CI actions are commit-pinned, write permission is limited to the publish job, and release archives include `SHA256SUMS`.

Terminal output and response redirect files can contain authorization headers, cookies, tokens and private response data. Review them before sharing.

The extension capability manifest restricts downloads to this project's GitHub releases.

## Supported platforms

Release bundles are built for:

- macOS arm64 and x86-64
- Linux arm64 and x86-64
- Windows arm64 and x86-64

## Development

### Gutter tasks

The run arrow comes from the extension's runnable query (`languages/http/runnables.scm`) and tagged task (`languages/http/tasks.json`). On macOS and Linux, the task launcher resolves the binary from Zed's installed extension without requiring it on `PATH`. A dev extension uses its checkout's `target/debug/zed-http-lsp`; a released extension uses its matching downloaded binary. Set `ZED_HTTP_LSP` in the task environment to use another binary. In this checkout, the project tasks use `cargo run` to build and start the local binary. Tasks explicitly select `/bin/sh`, so they behave the same when your default terminal shell is Fish, Bash or Zsh. File paths are passed through environment variables and quoted by the launcher command.

JSON is indented and syntax-highlighted when stdout is a terminal. `NO_COLOR=1` disables color; `FORCE_COLOR=1` enables it for captured output.

On Windows, define the tasks in the project's `.zed/tasks.json` with `command` set to the absolute path of `zed-http-lsp.exe` and the `--run`, `$ZED_FILE`, `--line`, `$ZED_ROW` arguments (omit the line arguments for Send All).

### Local build

Build the binary (`cargo setup` is an alias for this):

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

### Testing on macOS

`zed-http-lsp` supports Apple Silicon and Intel Macs (macOS 11 or later). On a Mac, run the tests and the end-to-end smoke test:

```bash
cargo test --workspace
cargo build -p zed-http-lsp && node scripts/smoke_lsp.cjs
```

To try a release build the way the extension ships it, build and ad-hoc sign it:

```bash
cargo build --release -p zed-http-lsp
codesign --force --sign - target/release/zed-http-lsp
codesign --verify --verbose target/release/zed-http-lsp
```

Then install this checkout with **zed: install dev extension** and point `lsp.zed-http-lsp.binary.path` at `target/release/zed-http-lsp` (or `target/debug/zed-http-lsp`), as shown above.

Zed downloads release binaries into `~/Library/Application Support/Zed/extensions/work/http/zed-http-lsp-<target>-<version>/`. The current and the previous version are kept, so a language server started before an update keeps working until it restarts. If macOS refuses to run a downloaded binary, check it for a quarantine attribute and remove it:

```bash
xattr -l ~/Library/Application\ Support/Zed/extensions/work/http/zed-http-lsp-*/zed-http-lsp
xattr -d com.apple.quarantine ~/Library/Application\ Support/Zed/extensions/work/http/zed-http-lsp-*/zed-http-lsp
```

On macOS, HTTPS, WebSocket and gRPC certificates are verified against the system Keychain (through rustls-platform-verifier), so certificates trusted there, including corporate roots, are accepted.
