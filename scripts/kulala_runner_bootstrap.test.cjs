const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const {
    CLI_VERSION,
    CORE_VERSION,
    coreAsset,
    defaultJetBrainsEnvironment,
    hasJetBrainsEnvironmentConfig,
    platformKey,
    runnerLayout,
    sha256File,
    withAutomaticEnvironmentSelection,
} = require("./kulala_runner_bootstrap.cjs");

test("maps Node platform names to Kulala release assets", () => {
    assert.equal(platformKey("darwin", "arm64"), "darwin-arm64");
    assert.equal(platformKey("linux", "arm64"), "linux-aarch64");
    assert.equal(platformKey("linux", "x64"), "linux-x86_64");
    assert.equal(platformKey("win32", "x64"), "windows-x86_64");
    assert.equal(coreAsset("darwin", "arm64").name, "kulala-core-darwin-arm64");
    assert.throws(() => coreAsset("freebsd", "x64"), /does not provide a binary/);
});

test("places the pinned CLI and Core under the extension cache", () => {
    const layout = runnerLayout({
        env: { XDG_CACHE_HOME: "/tmp/zed-http-test-cache" },
        platform: "linux",
        arch: "x64",
        homeDir: "/unused",
    });

    assert.match(layout.root, new RegExp(`kulala-cli-${CLI_VERSION}$`));
    assert.match(layout.coreRoot, new RegExp(`kulala-core-${CORE_VERSION}-linux-x86_64$`));
    assert.equal(path.basename(layout.cliPath), "cli.cjs");
    assert.equal(path.basename(layout.corePath), "kulala-core");
});

test("hashes downloaded artifacts without loading them as one buffer", async () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "zed-http-hash-test-"));
    const fixture = path.join(directory, "fixture");
    try {
        fs.writeFileSync(fixture, "zed-http\n");
        assert.equal(
            await sha256File(fixture),
            "7b7ccb14c7c7c1413a8b347621120461c2d646200754d212ee1d7e34b9504483",
        );
    } finally {
        fs.rmSync(directory, { recursive: true, force: true });
    }
});

test("selects a stable IntelliJ environment without prompting or changing the HTTP file", () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "zed-http-env-test-"));
    const requestsDirectory = path.join(directory, "http", "nested");
    const requestPath = path.join(requestsDirectory, "requests.http");
    try {
        fs.mkdirSync(requestsDirectory, { recursive: true });
        fs.writeFileSync(
            path.join(directory, "http-client.env.json"),
            '{"dev-admin": {}, "dev-member": {}}\n',
        );
        fs.writeFileSync(requestPath, "GET {{baseUrl}}/api/auth/ok\n");

        assert.equal(hasJetBrainsEnvironmentConfig(requestPath), true);
        assert.deepEqual(withAutomaticEnvironmentSelection(["run", requestPath, "--line", "1"]), [
            "run",
            requestPath,
            "--line",
            "1",
            "--env",
            "dev-admin",
        ]);
        assert.equal(fs.readFileSync(requestPath, "utf8"), "GET {{baseUrl}}/api/auth/ok\n");
    } finally {
        fs.rmSync(directory, { recursive: true, force: true });
    }
});

test("does not prompt when no IntelliJ environment file is present", () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "zed-http-no-env-test-"));
    const requestPath = path.join(directory, "requests.http");
    try {
        fs.writeFileSync(requestPath, "GET https://example.com\n");
        assert.deepEqual(withAutomaticEnvironmentSelection(["run", requestPath]), [
            "run",
            requestPath,
        ]);
    } finally {
        fs.rmSync(directory, { recursive: true, force: true });
    }
});

test("prefers an environment named default over declaration order", () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "zed-http-default-env-test-"));
    const requestPath = path.join(directory, "requests.http");
    try {
        fs.writeFileSync(requestPath, "GET {{baseUrl}}\n");
        fs.writeFileSync(
            path.join(directory, "http-client.env.json"),
            '{"staging": {}, "default": {}, "production": {}}\n',
        );
        assert.equal(defaultJetBrainsEnvironment(requestPath), "default");
        assert.deepEqual(withAutomaticEnvironmentSelection(["run", requestPath]), [
            "run",
            requestPath,
            "--env",
            "default",
        ]);
    } finally {
        fs.rmSync(directory, { recursive: true, force: true });
    }
});

test("honors explicit and non-interactive environment choices", () => {
    assert.deepEqual(
        withAutomaticEnvironmentSelection(["run", "requests.http", "--env", "dev-member"], {
            env: { ZED_HTTP_ENV: "dev-admin" },
        }),
        ["run", "requests.http", "--env", "dev-member"],
    );
    assert.deepEqual(
        withAutomaticEnvironmentSelection(["run", "requests.http"], {
            env: { ZED_HTTP_ENV: " dev-admin " },
        }),
        ["run", "requests.http", "--env", "dev-admin"],
    );
});
