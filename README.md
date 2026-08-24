> [!WARNING]
> **Experimental and a heavy work in progress.** This extension is an exploration of IntelliJ-compatible HTTP request support in Zed. Compatibility, installation behavior, cache formats, and configuration may change without notice. Do not rely on it for production-critical workflows yet.

# HTTP extension for Zed

`zed-http` adds language support and runnable HTTP requests to Zed. Its goal is to let a project share JetBrains-style `.http` files between IntelliJ-based IDEs and Zed without maintaining editor-specific request files.

The extension currently combines the Kulala language server with a pinned Kulala CLI/Core runner. It is not affiliated with JetBrains or the Kulala project.

## Features

- Syntax highlighting for HTTP methods, headers, URLs, variables, and request bodies
- Completions for methods, URL schemes, HTTP versions, headers, and variables
- Environment-variable completion from `http-client.env.json`, `http-client.private.env.json`, and `.env`
- Runnable markers for individual requests
- A task for running every request in the current file
- JetBrains-style pre-request and response-handler scripts
- Persistent `client.global` variables and cookies across separate request runs
- Automatic environment selection without prompting on every request

## Trying a development checkout

This experimental version should be installed as a Zed development extension:

1. Open Zed's command palette.
2. Run **zed: install dev extension**.
3. Select this repository's directory.
4. Reload the extension or restart Zed after changing the generated language tasks.

The language server is downloaded into Zed's extension-managed storage. Running requests additionally requires Node.js 20+ and `npm` on your `PATH`.

## Running requests

Place the cursor in a request and use its runnable marker to invoke **Run HTTP request at cursor**. The selected request is determined from Zed's current row. To execute the complete file, choose **Run all HTTP requests in file** from Zed's task picker.

The first request run:

1. Installs the pinned `@mistweaverco/kulala-cli@0.16.0` package without lifecycle scripts.
2. Downloads the matching Kulala Core `0.36.0` executable.
3. Verifies the executable against a pinned SHA-256 digest.
4. Reuses and re-verifies the cached installation on later runs.

Nothing is installed into the project or as a global npm package. The runner cache is stored under `${XDG_CACHE_HOME:-$HOME/.cache}/zed-http/kulala-cli-0.16.0` on macOS and Linux, or the equivalent local application-data directory on Windows.

## Environments

The runner looks from the request file's directory towards the filesystem root for the nearest directory containing either of these standard JetBrains files:

- `http-client.env.json`
- `http-client.private.env.json`

It selects an environment without showing a prompt. The priority is:

1. The environment named by `ZED_HTTP_ENV`, when set.
2. An environment named `default`.
3. The first environment declared in the environment file.

The files themselves remain unchanged. Environment selection supplies configuration such as `baseUrl`, credentials, and seeded IDs; it does not determine which authenticated session remains active after a login response handler runs.

For example:

```json
{
  "default": {
    "baseUrl": "http://localhost:3000",
    "email": "admin@example.test",
    "password": "dev-password"
  },
  "member": {
    "baseUrl": "http://localhost:3000",
    "email": "member@example.test",
    "password": "dev-password"
  }
}
```

Keep secrets in `http-client.private.env.json` and exclude that file from version control.

## Authentication and request chaining

JetBrains-compatible response handlers can store a login token for later requests:

```http
### Login
# @name login
POST {{baseUrl}}/api/auth/sign-in/email
Content-Type: application/json

{
  "email": "{{email}}",
  "password": "{{password}}"
}

> {%
    client.global.set("authToken", response.body.token);
%}

### Get session
GET {{baseUrl}}/api/auth/get-session
Authorization: Bearer {{authToken}}
```

Run **Login** once and later request runs can resolve `{{authToken}}`. If the file contains separate requests such as **Login as admin** and **Login as member**, running either request replaces the same persisted `authToken`; subsequent requests therefore use the most recently authenticated user.

Kulala Core persists `client.global` values and cookies in its OS-specific application-data directory, so they survive the separate processes created by Zed tasks.

## Language server

The extension downloads `@mistweaverco/kulala-ls` into Zed's extension-managed storage and starts it with Zed's managed Node.js runtime. No project dependency or global installation is required for language features.

The language server currently provides completion and hover capabilities. Request execution is implemented separately because the language server does not expose an LSP command for running requests.

## Requirements and limitations

- Request execution currently requires a POSIX `/bin/sh` task shell.
- Node.js 20+ and `npm` must be available on `PATH`.
- The first request requires network access to the npm registry and GitHub Releases.
- Kulala Core publishes macOS arm64/x86-64, Linux arm64/x86-64, and Windows x86-64 binaries, but the current POSIX task wrapper does not yet provide native Windows task support.
- Kulala aims for JetBrains HTTP Client compatibility, but full IntelliJ feature parity is not guaranteed. Unsupported syntax and behavioral differences should be treated as bugs in this experiment.

Kulala's current macOS release binaries have an [upstream ad-hoc signing issue](https://github.com/mistweaverco/kulala-core/issues/178). After verifying the upstream digest, the bootstrap repairs the cached copy with `/usr/bin/codesign` and verifies the repaired signature. This workaround should be removed once upstream releases pass their signing gate.

## Security

`.http` pre-request and response-handler scripts execute as trusted code. Only run request files you trust. Request output may contain authorization headers, cookies, tokens, or response data; review terminal output before sharing it.

The bootstrap installs the CLI with npm lifecycle scripts disabled and verifies the separately downloaded Core executable against platform-specific pinned checksums.

## Development

Zed currently does not expose extension-managed executable paths to language task templates, so the generated task embeds the installer/launcher. The readable source is [`scripts/kulala_runner_bootstrap.cjs`](scripts/kulala_runner_bootstrap.cjs); do not edit the encoded payload in `languages/http/tasks.json` directly.

Regenerate and verify the task artifact with:

```bash
node scripts/generate_http_tasks.mjs --write
node scripts/generate_http_tasks.mjs --check
node --test scripts/kulala_runner_bootstrap.test.cjs
node scripts/smoke_kulala_runner.cjs
cargo fmt --check
cargo test
```
