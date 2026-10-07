const assert = require("node:assert/strict");
const childProcess = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const { fileURLToPath, pathToFileURL } = require("node:url");

const repositoryRoot = path.join(__dirname, "..");
// The server runs with a temporary working directory, so a relative override must be resolved
// against ours first.
const lspPath = path.resolve(process.env.ZED_HTTP_LSP ?? path.join(
    repositoryRoot,
    "target",
    "debug",
    process.platform === "win32" ? "zed-http-lsp.exe" : "zed-http-lsp",
));
const SHUTDOWN_DEADLINE_MS = 10_000;

class LspClient {
    constructor(command, options) {
        this.child = childProcess.spawn(command, [], {
            ...options,
            stdio: ["pipe", "pipe", "pipe"],
        });
        this.buffer = Buffer.alloc(0);
        this.nextId = 1;
        this.pending = new Map();
        this.notifications = [];
        this.waiters = [];
        this.stderr = "";
        this.child.stderr.setEncoding("utf8");
        this.child.stderr.on("data", (chunk) => (this.stderr += chunk));
        this.child.stdout.on("data", (chunk) => this.receive(chunk));
        this.exited = false;
        // A server that fails to start or dies fails every outstanding request immediately.
        this.child.on("error", (error) => this.fail(new Error(`failed to start ${command}: ${error.message}`)));
        this.child.stdin.on("error", (error) => this.fail(new Error(`server stdin closed: ${error.message}\n${this.stderr}`)));
        this.child.on("exit", (code, signal) => {
            this.exited = true;
            this.fail(new Error(`server exited (code ${code}, signal ${signal})\n${this.stderr}`));
        });
    }

    fail(error) {
        for (const [id, pending] of this.pending) {
            clearTimeout(pending.timer);
            pending.reject(error);
            this.pending.delete(id);
        }
    }

    send(message) {
        const body = Buffer.from(JSON.stringify(message));
        this.child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
        this.child.stdin.write(body);
    }

    request(method, params) {
        const id = this.nextId++;
        this.send({ jsonrpc: "2.0", id, method, params });
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this.pending.delete(id);
                reject(new Error(`timed out waiting for ${method}\n${this.stderr}`));
            }, 15_000);
            this.pending.set(id, { resolve, reject, timer });
        });
    }

    notify(method, params) {
        this.send({ jsonrpc: "2.0", method, params });
    }

    receive(chunk) {
        this.buffer = Buffer.concat([this.buffer, chunk]);
        while (true) {
            const headerEnd = this.buffer.indexOf("\r\n\r\n");
            if (headerEnd < 0) return;
            const header = this.buffer.subarray(0, headerEnd).toString("ascii");
            const length = Number(header.match(/Content-Length: (\d+)/i)?.[1]);
            if (!Number.isInteger(length)) throw new Error(`invalid LSP header: ${header}`);
            const messageEnd = headerEnd + 4 + length;
            if (this.buffer.length < messageEnd) return;
            const message = JSON.parse(this.buffer.subarray(headerEnd + 4, messageEnd));
            this.buffer = this.buffer.subarray(messageEnd);
            this.handle(message);
        }
    }

    // Resolves with the first notification of `method` that satisfies `predicate`.
    waitForNotification(method, predicate = () => true) {
        const seen = this.notifications.find((message) => message.method === method && predicate(message.params));
        if (seen) return Promise.resolve(seen.params);
        return new Promise((resolve, reject) => {
            const timer = setTimeout(
                () => reject(new Error(`timed out waiting for ${method}\n${this.stderr}`)),
                15_000,
            );
            this.waiters.push({ method, predicate, resolve: (params) => (clearTimeout(timer), resolve(params)) });
        });
    }

    handle(message) {
        if (message.method && message.id !== undefined) {
            this.send({ jsonrpc: "2.0", id: message.id, result: null });
            return;
        }

        if (message.method) {
            this.notifications.push(message);
            this.waiters = this.waiters.filter((waiter) => {
                if (waiter.method !== message.method || !waiter.predicate(message.params)) return true;
                waiter.resolve(message.params);
                return false;
            });
            return;
        }

        if (message.id !== undefined && !message.method) {
            const pending = this.pending.get(message.id);
            if (!pending) return;
            clearTimeout(pending.timer);
            this.pending.delete(message.id);
            if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
            else pending.resolve(message.result);
        }
    }

    async stop() {
        const closed = this.exited
            ? Promise.resolve()
            : new Promise((resolve) => this.child.once("close", resolve));
        let timer;
        const deadline = new Promise((_, reject) => {
            timer = setTimeout(
                () => reject(new Error(`server did not shut down within ${SHUTDOWN_DEADLINE_MS} ms`)),
                SHUTDOWN_DEADLINE_MS,
            );
        });
        try {
            await Promise.race([
                (async () => {
                    await this.request("shutdown");
                    this.notify("exit");
                    this.child.stdin.end();
                    await closed;
                })(),
                deadline,
            ]);
        } finally {
            clearTimeout(timer);
        }
    }
}

// A minimal WebSocket echo endpoint (RFC 6455 text frames only), so the smoke test needs no
// dependencies.
function acceptWebSocket(request, socket) {
    const accept = crypto
        .createHash("sha1")
        .update(`${request.headers["sec-websocket-key"]}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`)
        .digest("base64");
    socket.write(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            + `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    const frame = (opcode, payload) => {
        const header = payload.length < 126
            ? Buffer.from([0x80 | opcode, payload.length])
            : Buffer.from([0x80 | opcode, 126, payload.length >> 8, payload.length & 0xff]);
        return Buffer.concat([header, payload]);
    };
    let buffer = Buffer.alloc(0);
    socket.on("data", (chunk) => {
        buffer = Buffer.concat([buffer, chunk]);
        while (buffer.length >= 2) {
            const opcode = buffer[0] & 0x0f;
            let length = buffer[1] & 0x7f;
            let offset = 2;
            if (length === 126) {
                if (buffer.length < 4) return;
                length = buffer.readUInt16BE(2);
                offset = 4;
            } else if (length === 127) {
                socket.destroy();
                return;
            }
            const masked = (buffer[1] & 0x80) !== 0;
            const end = offset + (masked ? 4 : 0) + length;
            if (buffer.length < end) return;
            const mask = masked ? buffer.subarray(offset, offset + 4) : Buffer.alloc(4);
            const payload = Buffer.from(buffer.subarray(end - length, end));
            for (let index = 0; index < payload.length; index += 1) payload[index] ^= mask[index % 4];
            buffer = buffer.subarray(end);
            if (opcode === 0x1) {
                socket.write(frame(0x1, Buffer.from(`echo: ${payload.toString("utf8")}`)));
            } else if (opcode === 0x8) {
                socket.end(frame(0x8, payload.subarray(0, 2)));
                return;
            }
        }
    });
    socket.on("error", () => {});
}

// Runs a gutter task the way Zed does: in the workspace root, with a one-based row. It must not
// block, because the mock server answers from this process.
function runTask(cwd, file, row) {
    const args = ["--run", file];
    if (row !== undefined) args.push("--line", String(row));
    return new Promise((resolve, reject) => {
        const child = childProcess.execFile(
            lspPath,
            args,
            { cwd, env: { ...process.env, NO_COLOR: "1" }, encoding: "utf8", timeout: 30_000 },
            (error, stdout, stderr) => {
                if (error && typeof error.code !== "number") reject(error);
                else resolve({ status: child.exitCode, stdout, stderr });
            },
        );
    });
}

async function main() {
    assert.ok(fs.existsSync(lspPath), `native LSP not found at ${lspPath}`);
    const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "zed-http-lsp-smoke-"));
    const seen = [];
    const server = http.createServer((request, response) => {
        seen.push({ url: request.url, authorization: request.headers.authorization });
        response.setHeader("Content-Type", "application/json");
        if (request.url === "/graphql") {
            let body = "";
            request.setEncoding("utf8");
            request.on("data", (chunk) => (body += chunk));
            request.on("end", () => response.end(JSON.stringify({ data: JSON.parse(body) })));
            return;
        }
        if (request.url === "/prepared") {
            response.end(JSON.stringify({ prepared: true }));
            return;
        }
        const authorized = request.headers.authorization === "Bearer native-runner-token";
        response.statusCode = authorized ? 200 : 401;
        response.end(JSON.stringify({ authorized }));
    });

    // Upgraded sockets are not tracked by server.close(), so destroy them explicitly.
    const sockets = new Set();
    server.on("upgrade", (request, socket) => {
        sockets.add(socket);
        socket.once("close", () => sockets.delete(socket));
        seen.push({ url: request.url, authorization: request.headers.authorization });
        acceptWebSocket(request, socket);
    });

    let client;
    try {
        await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
        const port = server.address().port;
        const requestPath = path.join(temporary, "requests.http");
        const source = [
            "### Prepare with a script",
            "< {%",
            '  request.variables.set("dynamicPath", "prepared");',
            "%}",
            "GET {{baseUrl}}/{{dynamicPath}}",
            "",
            '> {% client.global.set("sessionToken", "native-runner-token"); %}',
            "",
            "### Verify the session",
            "GET {{baseUrl}}/verify",
            "Authorization: Bearer {{sessionToken}}",
            "X-Env: {{smokeToken}}",
            "",
            "### GraphQL",
            "GRAPHQL {{baseUrl}}/graphql",
            "",
            "query Smoke {",
            "  ok",
            "}",
            "",
            '{ "token": "{{smokeToken}}" }',
            "",
            "### WebSocket",
            "# @timeout 2s",
            "WEBSOCKET {{wsUrl}}/socket",
            "",
            "===",
            "hello {{smokeToken}}",
            "",
        ].join("\n");
        fs.writeFileSync(requestPath, source);
        fs.writeFileSync(
            path.join(temporary, "http-client.env.json"),
            `${JSON.stringify({
                smoke: {
                    baseUrl: `http://127.0.0.1:${port}`,
                    smokeToken: "native-runner-token",
                    wsUrl: `ws://127.0.0.1:${port}`,
                },
            }, null, 2)}\n`,
        );

        client = new LspClient(lspPath, {
            cwd: temporary,
            env: { ...process.env, ZED_HTTP_ENV: "smoke" },
        });
        const uri = pathToFileURL(requestPath).href;
        const initialized = await client.request("initialize", {
            processId: process.pid,
            capabilities: {},
            rootUri: pathToFileURL(temporary).href,
        });
        assert.ok(initialized.capabilities.completionProvider, JSON.stringify(initialized));
        assert.ok(initialized.capabilities.hoverProvider, JSON.stringify(initialized));
        assert.equal(initialized.capabilities.codeLensProvider, undefined);
        client.notify("initialized", {});
        client.notify("textDocument/didOpen", {
            textDocument: { uri, languageId: "http", version: 1, text: source },
        });

        const diagnostics = await client.waitForNotification(
            "textDocument/publishDiagnostics",
            (params) => params.uri === uri,
        );
        assert.equal(diagnostics.diagnostics.length, 0, JSON.stringify(diagnostics));

        // Completion inside `{{` offers env, script-set and dynamic variables.
        const envLine = source.split("\n").findIndex((line) => line.startsWith("X-Env:"));
        const completions = await client.request("textDocument/completion", {
            textDocument: { uri },
            position: { line: envLine, character: "X-Env: {{".length },
        });
        const labels = completions.map((item) => item.label);
        for (const expected of ["smokeToken", "baseUrl", "sessionToken", "$uuid"]) {
            assert.ok(labels.includes(expected), `missing ${expected}: ${labels}`);
        }

        const hoverAt = (prefix) => client.request("textDocument/hover", {
            textDocument: { uri },
            position: {
                line: source.split("\n").findIndex((line) => line.startsWith(prefix)),
                character: prefix.length + 3,
            },
        });
        const envHover = await hoverAt("X-Env: ");
        assert.match(envHover.contents.value, /environment `smoke`/);
        assert.match(envHover.contents.value, /native-runner-token/);
        assert.match((await hoverAt("Authorization: Bearer ")).contents.value, /set by a script/);

        // Gutter tasks are forwarded to this server once it has bound its socket.
        const lines = source.split("\n");
        const row = (prefix) => lines.findIndex((line) => line.startsWith(prefix)) + 1;
        let prepared;
        for (let attempt = 0; attempt < 50; attempt += 1) {
            prepared = await runTask(temporary, requestPath, row("GET {{baseUrl}}/{{dynamicPath}}"));
            if (prepared.stderr === "") break;
            await new Promise((resolve) => setTimeout(resolve, 100));
        }
        assert.equal(prepared.status, 0, prepared.stderr);
        assert.equal(prepared.stderr, "", "the run was not forwarded to the language server");
        assert.match(prepared.stdout, /200 OK/);
        assert.match(prepared.stdout, /"prepared": true/);
        assert.equal(seen.at(-1).url, "/prepared");

        // The token a handler stored in the previous run is used by this one.
        const verified = await runTask(temporary, requestPath, row("GET {{baseUrl}}/verify"));
        assert.equal(verified.status, 0, verified.stdout + verified.stderr);
        assert.match(verified.stdout, /"authorized": true/);
        assert.equal(seen.at(-1).authorization, "Bearer native-runner-token");
        // Hover reads the same in-memory session.
        assert.match((await hoverAt("Authorization: Bearer ")).contents.value, /client\.global/);

        const graphql = await runTask(temporary, requestPath, row("GRAPHQL"));
        assert.equal(graphql.status, 0, graphql.stdout + graphql.stderr);
        assert.match(graphql.stdout, /"operationName": "Smoke"/);
        assert.match(graphql.stdout, /"token": "native-runner-token"/);

        const websocket = await runTask(temporary, requestPath, row("WEBSOCKET"));
        assert.equal(websocket.status, 0, websocket.stdout + websocket.stderr);
        assert.match(websocket.stdout, /→ hello native-runner-token/);
        assert.match(websocket.stdout, /← echo: hello native-runner-token/);
        assert.equal(seen.at(-1).url, "/socket");

        await client.stop();
        client = undefined;
        console.log("Smoke test passed: completion, hover and diagnostics work, and gutter runs share the language server's session.");
    } finally {
        if (client) client.child.kill();
        for (const socket of sockets) socket.destroy();
        server.closeAllConnections?.();
        await new Promise((resolve) => server.close(resolve));
        fs.rmSync(temporary, { recursive: true, force: true });
    }
}

main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
});
