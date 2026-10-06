const assert = require("node:assert/strict");
const childProcess = require("node:child_process");
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const { fileURLToPath, pathToFileURL } = require("node:url");

const repositoryRoot = path.join(__dirname, "..");
const lspPath = process.env.ZED_HTTP_LSP ?? path.join(
    repositoryRoot,
    "target",
    "debug",
    process.platform === "win32" ? "zed-http-lsp.exe" : "zed-http-lsp",
);

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
        await this.request("shutdown");
        const closed = new Promise((resolve) => this.child.once("close", resolve));
        this.notify("exit");
        this.child.stdin.end();
        await closed;
    }
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
        ].join("\n");
        fs.writeFileSync(requestPath, source);
        fs.writeFileSync(
            path.join(temporary, "http-client.env.json"),
            `${JSON.stringify({
                smoke: {
                    baseUrl: `http://127.0.0.1:${port}`,
                    smokeToken: "native-runner-token",
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
        assert.equal(sendLenses.length, 3, JSON.stringify(lenses));

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

        await client.stop();
        client = undefined;
        console.log("Native runner smoke test passed: code lenses execute unsaved requests in-process.");
    } finally {
        if (client) client.child.kill();
        await new Promise((resolve) => server.close(resolve));
        fs.rmSync(temporary, { recursive: true, force: true });
    }
}

main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
});
