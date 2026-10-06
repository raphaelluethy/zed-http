# Native HTTP runner in `zed-http-lsp` (replacing Kulala Core)

## Context

Request execution depended on Kulala Core 0.36.0 binaries from `mistweaverco/kulala-core`. That repo has been deleted, no mirror has byte-identical assets, and the npm package only downloads from the dead repo. So no release can be built, and users get the 404 popup. The fix is to make `zed-http-lsp` self-contained: it parses and executes IntelliJ-style `.http` files itself, with no sidecar process. The release archive then contains a single binary, and code lenses no longer spawn a process on every edit.

Decisions confirmed by the user:
- **Scripts:** embedded Boa (pure-Rust JS engine).
- **Protocols:** full scope (HTTP, GraphQL, WebSocket, gRPC).
- **Session state:** `client.global` values and cookies are kept in memory only.

## Architecture

The LSP surface stays as it is: the lenses, commands, response files and background execution in `http-lsp/src/backend.rs`. Only `core.rs` (the sidecar client) is replaced by native modules.

```
http-lsp/src/
  main.rs          (unchanged)
  backend.rs       swap CoreClient → Runner; parse natively for code lenses
  syntax.rs        .http parser → Document { blocks, file_vars, imports, errors }
  variables.rs     env files, $shared, @vars, dynamic vars, {{ }} substitution
  session.rs       in-memory globals, cookie jar, named-response store (Arc, shared)
  runner.rs        orchestrates a run: select blocks → pre-script → substitute → send → handler → report
  report.rs        Report/Execution model + render (moved from core.rs; render code reused as-is)
  script.rs        Boa runtime + IntelliJ client/request/response API
  protocol/http.rs     reqwest (rustls): methods, bodies, multipart, redirects, cookies, timeouts
  protocol/graphql.rs  GRAPHQL → POST {query, variables, operationName}
  protocol/websocket.rs tokio-tungstenite: === message frames, wait-for-server, bounded collection
  protocol/grpc.rs     tonic + prost-reflect: server reflection or .proto via protox, JSON ↔ DynamicMessage
```

**Reused as-is:**
- `core.rs`: the report types and rendering (`CoreReport::render`, `CoreExecution::render_into`, `ScriptConsoleEntry::render_into`, `RunSummary`, `OutputView`, `value_text`) move to `report.rs`. They lose their `Deserialize` derives and are built natively.
- `select_environment` (core.rs) moves to `variables.rs`.
- Everything in `backend.rs`: response file writing, `spawn_execute`, progress, the cache, and the `editor.action.goToLocations` lenses.

## Syntax supported (`syntax.rs`)

Line-based parser that records 0-based line numbers for each block. Each `RequestBlock` holds:
- `start_line` (the lens line)
- `name`: from `### Name`, or `# @name` / `// @name`, with the comment form taking precedence
- directives: `@no-redirect`, `@no-cookie-jar`, `@no-log`, `@timeout N`, `@connection-timeout N`
- `method`, which defaults to `GET`. Accepts HTTP verbs plus `GRAPHQL`, `WEBSOCKET`, `GRPC`.
- URL, which may continue on indented lines (`?a=1` / `&b=2`)
- optional `HTTP/1.1|HTTP/2`
- headers

**Body parts** run until `###` or until a handler or redirect line appears. A part is one of:
- inline text
- `< path`: raw file include
- `<@ path`: file include with variable substitution
- multipart sections delimited by `--boundary`, each of which can contain `< path`

**Pre-request script:** `< {% … %}` or `< path.js`, placed before the request line.

**Response handler:** `> {% … %}` or `> path.js`.

**Response redirect:** `>> path` (unique filename) or `>>! path` (overwrite).

**File-level syntax:**
- File variables: `@name = value`.
- `import ./other.http`: makes the other file's named requests available.
- `run #Name` / `run ./file.http`: executes a named request or another file.

Also: comments with `#` or `//`, and a stray `<` inside a body is not treated as an include unless it starts the line followed by a space.

Parse errors are collected per block (in place of `error_summary()`) and logged, as today.

## Variables (`variables.rs`)

Resolution order, highest first:
1. request vars set by the pre-script
2. `client.global`
3. file `@vars`
4. `http-client.private.env.json[env]`
5. `http-client.env.json[env]`
6. `$shared` from the private file
7. `$shared` from the public file

Env files are searched from the `.http` file's directory up to the workspace root. Environment selection keeps the current logic: `ZED_HTTP_ENV`, then `default`, then the alphabetically first name, skipping `$`-prefixed names.

**Dynamic variables:**
- `$uuid` / `$random.uuid`
- `$timestamp`, `$isoTimestamp`
- `$randomInt` / `$random.integer(a,b)`
- `$random.float(a,b)`, `$random.alphabetic(n)`, `$random.alphanumeric(n)`, `$random.hexadecimal(n)`, `$random.email`
- `$env.NAME` / `$processEnv NAME`

**Named response references** (Kulala / REST Client compatibility): `{{login.response.body.$.token}}` and `{{login.response.headers.X-Token}}`. These use JSONPath (`serde_json_path`) over the session's last response for that name.

Substitution is recursive with a depth/cycle guard. An unresolved `{{x}}` is left verbatim and reported as a warning in the response output.

## Scripts (`script.rs`, Boa)

Each script runs in a fresh `boa_engine::Context` inside `spawn_blocking`, because Boa is `!Send`. It is bounded by Boa runtime limits (loop iterations, recursion) plus a wall-clock guard. Inputs are passed in as JSON, and the script returns a `ScriptEffects { globals_set, globals_cleared, request_vars, logs, tests, exit }` that the runner applies. No filesystem or network access is exposed to scripts.

API surface (IntelliJ-compatible):
- `client`:
  - `client.global.set/get/isEmpty/clear/clearAll`
  - `client.test(name, fn)`
  - `client.assert(cond, msg)`
  - `client.log(...)`
  - `client.exit()`
- `request`:
  - `request.variables.set/get`
  - `request.environment.get`
  - `request.method/url/headers` (read-only in the pre-script)
- `response` (handler only):
  - `response.body` (parsed JSON when the content type is JSON, otherwise a string)
  - `response.headers.valueOf/valuesOf`
  - `response.status`
  - `response.contentType.mimeType/charset`
- Helpers: `jsonPath(obj, expr)`, `crypto.sha256/sha1/md5().updateWithText().digest().toHex()/toBase64()`, `$random`

The runner turns the effects into the report's `script_console` entries. The existing renderer already prints ✓/✗ test lines and `[script:level]` logs.

## Protocols

**HTTP (`reqwest`, rustls only, so no OpenSSL)**
- One client with a cookie store and one without, for `@no-cookie-jar`. Both share the session's `reqwest::cookie::Jar`.
- Redirect policy follows `@no-redirect`. `@timeout` and `@connection-timeout` are applied; the default timeout is 5 min.
- Responses are decompressed (gzip, brotli, deflate, zstd).
- Response bodies are capped at 32 MiB, as today.
- JSON bodies are pretty-printed. Binary responses render as `<binary N bytes, type>` unless they are redirected to a file.
- Timings are recorded (`timings.total`).

**GraphQL:** the body is the query, optionally followed by a JSON variables object after a blank line. It's sent as a `POST` with a JSON body.

**WebSocket (`tokio-tungstenite`, rustls)**
- The body is split on `===` lines. `=== wait-for-server` waits for one server message before the next frame is sent.
- Server messages are collected until 2 s of idle time after the last send, with a 30 s overall cap (both overridable with `@timeout`).
- The connection is then closed. Exchanged messages render as `→`/`←` lines in the response file.

**gRPC (`tonic` + `prost-reflect`)**
- `GRPC host:port/package.Service/Method`; the JSON body is the request message.
- Descriptors come from server reflection (v1, with v1alpha as fallback). If that fails, `.proto` files under the `.http` file's directory, up to the workspace root, are compiled with `protox` (pure Rust, no `protoc`).
- Unary and server-streaming calls are supported. Each response message renders as pretty JSON, and the gRPC status code and message are shown.
- TLS is used for `grpcs://` or `https://` URLs; otherwise plaintext.

## Backend changes (`backend.rs`)

- `core: Result<CoreClient, String>` is replaced by `runner: Runner` (holding an `Arc<Session>`). `initialized` logs "ready" without a Core path.
- `request_blocks` calls `syntax::parse(&text)` synchronously, and the parsed blocks are cached per document as now. This removes the per-edit process spawn.
- `execute` calls `runner.run(&path, &text, selected_line, env)`. Send selects the block whose range contains the line; Send All runs every block.
- The `ZED_HTTP_ENV` handling, execution lock, response files, progress and notifications stay unchanged.

## Extension, release and scripts

- `src/http.rs`:
  - drop `core_binary_name` from `PlatformAsset`
  - `is_installed` checks only the adapter binary
  - remove the `KULALA_CORE_PATH` env stripping
  - add `aarch64-pc-windows-msvc`, which the dropped Core no longer blocks
  - fix the error hint text and update the unit tests
- `.github/workflows/release.yml`: remove the `fetch_kulala_core` steps and package only `zed-http-lsp`. Add a `windows-11-arm` matrix entry. The verify job runs `cargo test` (integration tests included) and the smoke test without Core.
- Delete `scripts/fetch_kulala_core.cjs`. In `scripts/smoke_lsp.cjs`, drop the Core path logic and add a GraphQL and a WebSocket request.
- `README.md`: rewrite the Architecture, Features, Security and Development sections for the single native binary, and list known gaps against IntelliJ.
- `extension.toml`: update the description.

## Implementation order (each step leaves the workspace green)

1. `report.rs` (moved from core.rs), `syntax.rs`, `variables.rs`, `session.rs`, `protocol/http.rs`, `runner.rs`, and the backend switch. Delete `core.rs`. The smoke test passes for HTTP and env requests, apart from the script part.
2. `script.rs` (Boa): pre-request scripts and response handlers. The full existing smoke test passes.
3. Directives, multipart, file includes, response redirect, `import`/`run`, named response refs, GraphQL.
4. WebSocket.
5. gRPC (reflection and protox).
6. Extension, release workflow, scripts and README cleanup. Bump the version to 0.0.4 in all three manifests, as the version test enforces.

## Verification

- **Unit tests:**
  - parser fixtures in `http-lsp/tests/fixtures/*.http`: separators, names, multi-line URLs, bodies with `<`, multipart, scripts, redirects, imports
  - variable precedence and dynamic variables
  - script API behavior
  - report rendering (existing tests carried over)
- **Integration tests** (`http-lsp/tests/`) against in-process servers (dev-dependencies only):
  - an `axum` server for HTTP: redirects, cookies, gzip, multipart echo, GraphQL echo
  - a `tokio-tungstenite` echo server
  - a `tonic` server with reflection, generated through `tonic-build` + `protox` (no `protoc`)
- **Checks:** `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`, and `cargo build --target wasm32-wasip2`.
- **End to end:** `cargo build -p zed-http-lsp && node scripts/smoke_lsp.cjs`, which exercises the real LSP over stdio: lenses, Send, the script-driven path and env token.
- **In Zed:** install the dev extension, set `lsp.zed-http-lsp.binary.path` to `target/debug/zed-http-lsp`, open `test/test.http`, send requests, and check the Show/Headers/Save lenses and status-bar progress.
