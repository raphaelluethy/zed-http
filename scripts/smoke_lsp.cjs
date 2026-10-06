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
        this.executionWaiter = undefined;
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
        this.settleExecution(error);
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

    // Send and Send All run in the background; the server refreshes code lenses on success and
    // shows an error message on failure.
    execute(command) {
        const finished = new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this.executionWaiter = undefined;
                reject(new Error(`timed out waiting for ${command.command}\n${this.stderr}`));
            }, 30_000);
            this.executionWaiter = {
                resolve: () => (clearTimeout(timer), resolve()),
                reject: (error) => (clearTimeout(timer), reject(error)),
            };
        });
        return this.request("workspace/executeCommand", command).then(() => finished);
    }

    settleExecution(error) {
        const waiter = this.executionWaiter;
        this.executionWaiter = undefined;
        if (!waiter) return;
        if (error) waiter.reject(error);
        else waiter.resolve();
    }

    handle(message) {
        if (message.method && message.id !== undefined) {
            this.send({ jsonrpc: "2.0", id: message.id, result: null });
            if (message.method === "workspace/codeLens/refresh") this.settleExecution();
            return;
        }

        if (message.method === "window/showMessage" && message.params?.type === 1) {
            this.settleExecution(new Error(message.params.message));
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

function responseFromLens(lenses, sourceLine, title) {
    const lens = lenses.find(
        (candidate) =>
            candidate.range.start.line === sourceLine
            && candidate.command?.title === title
            && candidate.command?.command === "editor.action.goToLocations",
    );
    assert.ok(lens, `missing ${title} response lens on line ${sourceLine}: ${JSON.stringify(lenses)}`);
    const responseUri = lens.command.arguments?.[2]?.[0]?.uri;
    assert.ok(responseUri, `missing response URI: ${JSON.stringify(lens)}`);
    return fs.readFileSync(fileURLToPath(responseUri), "utf8");
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
            "### Verify the environment",
            "GET {{baseUrl}}/verify",
            "Authorization: Bearer {{smokeToken}}",
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
        await client.request("initialize", {
            processId: process.pid,
            capabilities: { window: { workDoneProgress: true } },
            rootUri: pathToFileURL(temporary).href,
        });
        client.notify("initialized", {});
        client.notify("textDocument/didOpen", {
            textDocument: { uri, languageId: "http", version: 1, text: source },
        });

        const lenses = await client.request("textDocument/codeLens", {
            textDocument: { uri },
        });
        const sendLenses = lenses.filter((lens) => lens.command?.command === "zed-http.send");
        assert.equal(sendLenses.length, 4, JSON.stringify(lenses));

        await client.execute(sendLenses[0].command);
        const loginLenses = await client.request("textDocument/codeLens", { textDocument: { uri } });
        const loginResponse = responseFromLens(
            loginLenses,
            sendLenses[0].range.start.line,
            "👁 Show",
        );
        assert.match(loginResponse, /HTTP\/1\.1 200 OK/);
        assert.match(loginResponse, /"prepared": true/);

        await client.execute(sendLenses[1].command);
        const verifyLenses = await client.request("textDocument/codeLens", { textDocument: { uri } });
        const verifyResponse = responseFromLens(
            verifyLenses,
            sendLenses[1].range.start.line,
            "👁 Show",
        );
        assert.match(verifyResponse, /HTTP\/1\.1 200 OK/);
        assert.match(verifyResponse, /"authorized": true/);
        assert.equal(seen[0].url, "/prepared");
        assert.equal(seen[1].authorization, "Bearer native-runner-token");

        await client.execute(sendLenses[2].command);
        const graphqlLenses = await client.request("textDocument/codeLens", { textDocument: { uri } });
        const graphqlResponse = responseFromLens(
            graphqlLenses,
            sendLenses[2].range.start.line,
            "👁 Show",
        );
        assert.match(graphqlResponse, /^# GRAPHQL http:\/\/127\.0\.0\.1:\d+\/graphql$/m);
        assert.match(graphqlResponse, /"operationName": "Smoke"/);
        assert.match(graphqlResponse, /"token": "native-runner-token"/);
        assert.equal(seen.length, 3);

        await client.execute(sendLenses[3].command);
        const websocketLenses = await client.request("textDocument/codeLens", { textDocument: { uri } });
        const websocketResponse = responseFromLens(
            websocketLenses,
            sendLenses[3].range.start.line,
            "👁 Show",
        );
        assert.match(websocketResponse, /^→ hello native-runner-token$/m);
        assert.match(websocketResponse, /^← echo: hello native-runner-token$/m);
        assert.equal(seen.length, 4);
        assert.equal(seen[3].url, "/socket");

        await client.stop();
        client = undefined;
        console.log("Native runner smoke test passed: code lenses execute unsaved requests in-process.");
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
