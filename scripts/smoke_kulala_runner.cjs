const assert = require("node:assert/strict");
const childProcess = require("node:child_process");
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");

const taskFile = path.join(__dirname, "..", "languages", "http", "tasks.json");
const taskArgument = JSON.parse(fs.readFileSync(taskFile, "utf8"))[0].args[1];
assert.match(taskArgument, /^"eval\(.+\)"$/);
const embeddedRunner = taskArgument.slice(1, -1);

function runRunner(args, options) {
    return new Promise((resolve, reject) => {
        const child = childProcess.spawn(process.execPath, ["-e", embeddedRunner, ...args], options);
        let stdout = "";
        let stderr = "";
        child.stdout.setEncoding("utf8");
        child.stderr.setEncoding("utf8");
        child.stdout.on("data", (chunk) => (stdout += chunk));
        child.stderr.on("data", (chunk) => (stderr += chunk));
        child.on("error", reject);
        child.on("close", (status, signal) => resolve({ status, signal, stdout, stderr }));
    });
}

async function main() {
    const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "zed-http-kulala-smoke-"));
    const seen = [];
    const server = http.createServer((request, response) => {
        seen.push({ url: request.url, authorization: request.headers.authorization, cookie: request.headers.cookie });
        response.setHeader("Content-Type", "application/json");
        if (request.url === "/login/admin") {
            response.setHeader("Set-Cookie", "zed_http_smoke_cookie=persisted; Path=/; HttpOnly");
            response.end(JSON.stringify({ token: "zed-http-admin-token" }));
            return;
        }
        if (request.url === "/login/member") {
            response.end(JSON.stringify({ token: "zed-http-member-token" }));
            return;
        }
        const expectedToken = request.url === "/verify/admin"
            ? "zed-http-admin-token"
            : "zed-http-member-token";
        const authorized =
            request.headers.authorization === `Bearer ${expectedToken}` &&
            request.headers.cookie?.includes("zed_http_smoke_cookie=persisted");
        response.statusCode = authorized ? 200 : 401;
        response.end(JSON.stringify({ authorized }));
    });

    try {
        await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
        const port = server.address().port;
        const requestFile = path.join(temporary, "persistence.http");
        const content = [
            "### Login as admin",
            "GET {{smokeBaseUrl}}/login/admin",
            "",
            "> {%",
            '    client.global.set("zedHttpSmokeToken", response.body.token);',
            "%}",
            "",
            "### Verify admin",
            "GET {{smokeBaseUrl}}/verify/admin",
            "Authorization: Bearer {{zedHttpSmokeToken}}",
            "",
            "### Login as member",
            "GET {{smokeBaseUrl}}/login/member",
            "",
            "> {%",
            '    client.global.set("zedHttpSmokeToken", response.body.token);',
            "%}",
            "",
            "### Verify member",
            "GET {{smokeBaseUrl}}/verify/member",
            "Authorization: Bearer {{zedHttpSmokeToken}}",
            "",
        ].join("\n");
        fs.writeFileSync(requestFile, content);
        fs.writeFileSync(
            path.join(temporary, "http-client.env.json"),
            `${JSON.stringify({ smoke: { smokeBaseUrl: `http://127.0.0.1:${port}` } }, null, 2)}\n`,
        );

        const childEnv = {
            ...process.env,
            XDG_CACHE_HOME: path.join(temporary, "cache"),
            npm_config_cache: path.join(temporary, "npm-cache"),
            ZED_HTTP_ENV: "smoke",
        };
        delete childEnv.HOME;
        delete childEnv.USERPROFILE;
        const options = { cwd: temporary, env: childEnv, stdio: ["ignore", "pipe", "pipe"] };

        const adminLogin = await runRunner(["run", requestFile, "--line", "2"], options);
        assert.equal(
            adminLogin.status,
            0,
            `admin login failed (${adminLogin.signal ?? "no signal"})\n${adminLogin.stderr}\n${adminLogin.stdout}`,
        );

        const adminVerify = await runRunner(["run", requestFile, "--line", "9"], options);
        assert.equal(
            adminVerify.status,
            0,
            `admin verification failed (${adminVerify.signal ?? "no signal"})\n${adminVerify.stderr}\n${adminVerify.stdout}`,
        );

        const memberLogin = await runRunner(["run", requestFile, "--line", "13"], options);
        assert.equal(
            memberLogin.status,
            0,
            `member login failed (${memberLogin.signal ?? "no signal"})\n${memberLogin.stderr}\n${memberLogin.stdout}`,
        );

        const memberVerify = await runRunner(["run", requestFile, "--line", "20"], options);
        assert.equal(
            memberVerify.status,
            0,
            `member verification failed (${memberVerify.signal ?? "no signal"})\n${memberVerify.stderr}\n${memberVerify.stdout}`,
        );
        assert.equal(seen.length, 4, `expected four selected requests, received ${JSON.stringify(seen)}`);
        assert.equal(seen[1].authorization, "Bearer zed-http-admin-token");
        assert.equal(seen[3].authorization, "Bearer zed-http-member-token");
        assert.match(seen[3].cookie ?? "", /zed_http_smoke_cookie=persisted/);
        console.log(
            "Kulala smoke test passed: environments, line selection, cookies, and latest-login token persistence work across runs.",
        );
    } finally {
        await new Promise((resolve) => server.close(resolve));
        fs.rmSync(temporary, { recursive: true, force: true });
    }
}

main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
});
