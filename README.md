# HTTP extension for Zed

## Overview

The `zed-http` extension adds syntax highlighting and language-server support for `.http` files in Zed. Its syntax follows the [JetBrains HTTP request format](https://github.com/JetBrains/http-request-in-editor-spec/blob/master/spec.md).

## Features

- Syntax highlighting for HTTP methods, headers, URLs, variables, and bodies
- Completions for methods, URL schemes, HTTP versions, headers, and variables
- Environment variable completion from `http-client.env.json`, `http-client.private.env.json`, and `.env`
- Runnable markers for individual requests and a task for running the entire file

## Language server

The extension automatically downloads `@mistweaverco/kulala-ls` into Zed's extension-managed storage and starts it with Zed's managed Node.js runtime. No project dependency or global installation is required.

The language server currently provides completion and hover capabilities. It does not implement an LSP command for executing requests.

## Running HTTP requests

The extension provides a language-level task bound to the `http-request` runnable tag. On the first run it installs the pinned `httpyac@6.16.7` runner into `${XDG_CACHE_HOME:-$HOME/.cache}/zed-http/httpyac-6.16.7`. Subsequent runs invoke the cached CLI directly with Node.js, avoiding `npx` resolution and registry checks. Nothing is installed into your project or as a global package. A separate **Run all HTTP requests in file** task is available from Zed's task picker.

Request execution requires a POSIX `/bin/sh` and Node.js 18+ with `npm` on your `PATH`. Zed currently does not expose extension-managed npm package paths to language task templates, so a small Node bootstrap manages the dedicated user cache. The task explicitly uses non-interactive `/bin/sh` to avoid loading interactive shell startup files on every request. The LSP itself remains downloaded and launched entirely through Zed's extension APIs.

### IntelliJ-compatible request chaining

Use JetBrains HTTP Client response handlers to store values needed by later requests:

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
# @ref login
GET {{baseUrl}}/api/auth/get-session
Authorization: Bearer {{authToken}}
```

`client.global.set("authToken", response.body.token)` and `{{authToken}}` are standard IntelliJ HTTP Client syntax. Avoid synthetic named-response expressions such as `{{login.response.body.token}}`, which are not part of the JetBrains request format.

Each Zed task invocation is a separate process, so global variables and cookies do not persist between individual runnable clicks. `# @ref login` tells httpyac to execute the named prerequisite in the same process before the selected request. IntelliJ treats `# @ref` as a normal comment; when using IntelliJ, run the named prerequisite once before running the dependent request.
