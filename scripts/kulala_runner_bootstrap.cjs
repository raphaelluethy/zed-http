const childProcess = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { Readable } = require("node:stream");
const { pipeline } = require("node:stream/promises");

const CLI_PACKAGE = "@mistweaverco/kulala-cli";
const CLI_VERSION = "0.16.0";
const CORE_VERSION = "0.36.0";
const CORE_RELEASE_BASE = `https://github.com/mistweaverco/kulala-core/releases/download/v${CORE_VERSION}`;

const CORE_ASSETS = Object.freeze({
    "darwin-arm64": {
        name: "kulala-core-darwin-arm64",
        sha256: "929f627b5e86dfff838c61b22a175a3cbc79637329f4ba268b94d181149073e4",
    },
    "darwin-x86_64": {
        name: "kulala-core-darwin-x86_64",
        sha256: "c12b0d4a5df1413ebe9b29535dc97d79585c4078c2091cf28b3c5efcc7d1c15b",
    },
    "linux-aarch64": {
        name: "kulala-core-linux-aarch64",
        sha256: "7275c3a165d03e3a8001fd6215a9f0e0d8473312afcafc216a5e1c4d336cf28a",
    },
    "linux-x86_64": {
        name: "kulala-core-linux-x86_64",
        sha256: "64034a012568a6b73fdc5fbc8d9665c9aa954dfa4a7c15fd46a52e49fbc19ed8",
    },
    "windows-x86_64": {
        name: "kulala-core-windows-x86_64.exe",
        sha256: "551883796d20bac1575e36d7b1b5e0e7fe28729b296fddb075d42c54aa4ec8df",
    },
});

const LOCK_STALE_AFTER_MS = 10 * 60 * 1000;
const LOCK_WAIT_MS = 15 * 60 * 1000;
const JETBRAINS_ENVIRONMENT_FILES = Object.freeze([
    "http-client.env.json",
    "http-client.private.env.json",
]);

function normalizedArch(arch) {
    if (arch === "x64") return "x86_64";
    if (arch === "arm64") return "aarch64";
    return arch;
}

function platformKey(platform = process.platform, arch = process.arch) {
    const normalizedPlatform = platform === "win32" ? "windows" : platform;
    let normalized = normalizedArch(arch);
    if (normalizedPlatform === "darwin" && normalized === "aarch64") {
        normalized = "arm64";
    }
    return `${normalizedPlatform}-${normalized}`;
}

function coreAsset(platform = process.platform, arch = process.arch) {
    const key = platformKey(platform, arch);
    const asset = CORE_ASSETS[key];
    if (!asset) {
        throw new Error(`Kulala Core ${CORE_VERSION} does not provide a binary for ${key}`);
    }
    return { ...asset, key };
}

function cacheHome(env = process.env, platform = process.platform, homeDir = os.homedir()) {
    if (env.XDG_CACHE_HOME) return env.XDG_CACHE_HOME;
    if (platform === "win32" && env.LOCALAPPDATA) return env.LOCALAPPDATA;
    return path.join(homeDir, ".cache");
}

function runnerLayout(options = {}) {
    const env = options.env ?? process.env;
    const platform = options.platform ?? process.platform;
    const arch = options.arch ?? process.arch;
    const homeDir = options.homeDir ?? os.homedir();
    const asset = coreAsset(platform, arch);
    const root = path.join(
        cacheHome(env, platform, homeDir),
        "zed-http",
        `kulala-cli-${CLI_VERSION}`,
    );
    const packageRoot = path.join(root, "node_modules", "@mistweaverco", "kulala-cli");
    const coreRoot = path.join(root, `kulala-core-${CORE_VERSION}-${asset.key}`);
    const executableName = platform === "win32" ? "kulala-core.exe" : "kulala-core";

    return {
        asset,
        root,
        packageRoot,
        cliPath: path.join(packageRoot, "dist", "cli.cjs"),
        packageJsonPath: path.join(packageRoot, "package.json"),
        coreRoot,
        corePath: path.join(coreRoot, executableName),
        coreMetadataPath: path.join(coreRoot, "install.json"),
        lockPath: path.join(root, ".install-lock"),
        npmCachePath: path.join(root, ".npm-cache"),
        coreCachePath: path.join(root, ".core-cache"),
    };
}

function findJetBrainsEnvironmentFiles(inputPath, cwd = process.cwd()) {
    if (typeof inputPath !== "string" || inputPath.length === 0) return [];

    const absoluteInputPath = path.resolve(cwd, inputPath);
    let directory;
    try {
        directory = fs.statSync(absoluteInputPath).isDirectory()
            ? absoluteInputPath
            : path.dirname(absoluteInputPath);
    } catch {
        directory = path.dirname(absoluteInputPath);
    }

    while (true) {
        const files = JETBRAINS_ENVIRONMENT_FILES.map((filename) =>
            path.join(directory, filename),
        ).filter((filePath) => fs.existsSync(filePath));
        if (files.length > 0) return files;

        const parent = path.dirname(directory);
        if (parent === directory) return [];
        directory = parent;
    }
}

function hasJetBrainsEnvironmentConfig(inputPath, cwd = process.cwd()) {
    return findJetBrainsEnvironmentFiles(inputPath, cwd).length > 0;
}

function defaultJetBrainsEnvironment(inputPath, cwd = process.cwd()) {
    const names = [];
    const seen = new Set();
    for (const filePath of findJetBrainsEnvironmentFiles(inputPath, cwd)) {
        const environments = readJson(filePath);
        if (!environments || typeof environments !== "object" || Array.isArray(environments)) continue;
        for (const name of Object.keys(environments)) {
            if (!seen.has(name)) {
                seen.add(name);
                names.push(name);
            }
        }
    }
    return names.find((name) => name === "default") ?? names[0];
}

function hasEnvironmentOption(args) {
    return args.some((argument) => argument === "--env" || argument.startsWith("--env="));
}

function withAutomaticEnvironmentSelection(args, options = {}) {
    const result = [...args];
    if (result[0] !== "run" || hasEnvironmentOption(result)) return result;

    const env = options.env ?? process.env;
    const configuredEnvironment = env.ZED_HTTP_ENV?.trim();
    if (configuredEnvironment) {
        result.push("--env", configuredEnvironment);
        return result;
    }

    const cwd = options.cwd ?? process.cwd();
    const defaultEnvironment = defaultJetBrainsEnvironment(result[1], cwd);
    if (defaultEnvironment) result.push("--env", defaultEnvironment);
    return result;
}

function readJson(filePath) {
    try {
        return JSON.parse(fs.readFileSync(filePath, "utf8"));
    } catch {
        return undefined;
    }
}

function cliReady(layout) {
    const manifest = readJson(layout.packageJsonPath);
    return manifest?.version === CLI_VERSION && fs.existsSync(layout.cliPath);
}

function sha256File(filePath) {
    return new Promise((resolve, reject) => {
        const hash = crypto.createHash("sha256");
        const input = fs.createReadStream(filePath);
        input.on("data", (chunk) => hash.update(chunk));
        input.on("error", reject);
        input.on("end", () => resolve(hash.digest("hex")));
    });
}

async function coreReady(layout) {
    if (!fs.existsSync(layout.corePath)) return false;
    const metadata = readJson(layout.coreMetadataPath);
    if (
        metadata?.coreVersion !== CORE_VERSION ||
        metadata?.asset !== layout.asset.name ||
        metadata?.sourceSha256 !== layout.asset.sha256 ||
        typeof metadata?.installedSha256 !== "string"
    ) {
        return false;
    }
    return (await sha256File(layout.corePath)) === metadata.installedSha256;
}

function commandError(command, result) {
    if (result.error) return result.error;
    const details = result.stderr?.toString().trim();
    return new Error(
        `${command} exited with ${result.status ?? `signal ${result.signal ?? "unknown"}`}${details ? `: ${details}` : ""}`,
    );
}

function runChecked(command, args, options = {}) {
    const result = childProcess.spawnSync(command, args, options);
    if (result.error || result.status !== 0) throw commandError(command, result);
    return result;
}

function installCli(layout, env = process.env) {
    fs.mkdirSync(layout.root, { recursive: true });
    console.error(`Installing ${CLI_PACKAGE}@${CLI_VERSION} in ${layout.root}...`);
    const npm = process.platform === "win32" ? "npm.cmd" : "npm";
    runChecked(
        npm,
        [
            "install",
            "--silent",
            "--no-audit",
            "--no-fund",
            "--ignore-scripts",
            "--prefix",
            layout.root,
            `${CLI_PACKAGE}@${CLI_VERSION}`,
        ],
        {
            stdio: "inherit",
            env: { ...env, npm_config_cache: layout.npmCachePath },
        },
    );
    if (!cliReady(layout)) {
        throw new Error(`npm did not install the expected ${CLI_PACKAGE}@${CLI_VERSION}`);
    }
}

async function downloadFile(url, destination) {
    const response = await fetch(url, { redirect: "follow" });
    if (!response.ok || !response.body) {
        throw new Error(`Failed to download ${url}: ${response.status} ${response.statusText}`);
    }
    await pipeline(Readable.fromWeb(response.body), fs.createWriteStream(destination, { flags: "wx" }));
}

function writeJsonAtomic(filePath, value) {
    const temporary = `${filePath}.${process.pid}.tmp`;
    fs.writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
    fs.renameSync(temporary, filePath);
}

function repairMacSignature(corePath) {
    const codesign = "/usr/bin/codesign";
    if (!fs.existsSync(codesign)) {
        throw new Error("Kulala Core requires /usr/bin/codesign on macOS");
    }
    runChecked(codesign, ["--force", "--sign", "-", corePath], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
    });
    runChecked(codesign, ["--verify", "--verbose=2", corePath], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
    });
}

async function installCore(layout, platform = process.platform) {
    fs.mkdirSync(layout.coreRoot, { recursive: true });
    const temporary = path.join(layout.coreRoot, `${layout.asset.name}.${process.pid}.download`);
    fs.rmSync(temporary, { force: true });
    const url = `${CORE_RELEASE_BASE}/${layout.asset.name}`;
    console.error(`Downloading and verifying Kulala Core ${CORE_VERSION} for ${layout.asset.key}...`);

    try {
        await downloadFile(url, temporary);
        const sourceSha256 = await sha256File(temporary);
        if (sourceSha256 !== layout.asset.sha256) {
            throw new Error(
                `Kulala Core checksum mismatch: expected ${layout.asset.sha256}, received ${sourceSha256}`,
            );
        }

        fs.chmodSync(temporary, 0o755);
        fs.rmSync(layout.corePath, { force: true });
        fs.renameSync(temporary, layout.corePath);

        let macosAdHocResigned = false;
        if (platform === "darwin") {
            repairMacSignature(layout.corePath);
            macosAdHocResigned = true;
        }

        const installedSha256 = await sha256File(layout.corePath);
        writeJsonAtomic(layout.coreMetadataPath, {
            cliPackage: CLI_PACKAGE,
            cliVersion: CLI_VERSION,
            coreVersion: CORE_VERSION,
            asset: layout.asset.name,
            sourceSha256,
            installedSha256,
            macosAdHocResigned,
        });
    } finally {
        fs.rmSync(temporary, { force: true });
    }
}

function delay(milliseconds) {
    return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function acquireInstallLock(layout) {
    fs.mkdirSync(layout.root, { recursive: true });
    const deadline = Date.now() + LOCK_WAIT_MS;

    while (Date.now() < deadline) {
        try {
            fs.mkdirSync(layout.lockPath);
            fs.writeFileSync(path.join(layout.lockPath, "owner"), `${process.pid}\n`);
            return () => fs.rmSync(layout.lockPath, { recursive: true, force: true });
        } catch (error) {
            if (error?.code !== "EEXIST") throw error;
            let age;
            try {
                age = Date.now() - fs.statSync(layout.lockPath).mtimeMs;
            } catch (statError) {
                if (statError?.code === "ENOENT") continue;
                throw statError;
            }
            if (age > LOCK_STALE_AFTER_MS) {
                fs.rmSync(layout.lockPath, { recursive: true, force: true });
                continue;
            }
            await delay(250);
        }
    }

    throw new Error(`Timed out waiting for Kulala installation lock ${layout.lockPath}`);
}

async function ensureRunner(options = {}) {
    const layout = runnerLayout(options);
    if (cliReady(layout) && (await coreReady(layout))) return layout;

    const releaseLock = await acquireInstallLock(layout);
    try {
        if (!cliReady(layout)) installCli(layout, options.env ?? process.env);
        if (!(await coreReady(layout))) {
            await installCore(layout, options.platform ?? process.platform);
        }
    } finally {
        releaseLock();
    }
    return layout;
}

async function main(args) {
    const layout = await ensureRunner();
    const runnerArgs = withAutomaticEnvironmentSelection(args);
    const result = childProcess.spawnSync(process.execPath, [layout.cliPath, ...runnerArgs], {
        stdio: "inherit",
        env: {
            ...process.env,
            KULALA_CORE_PATH: layout.corePath,
            KULALA_CORE_CACHE_DIR: layout.coreCachePath,
        },
    });
    if (result.error) throw result.error;
    if (result.status === null) {
        throw new Error(`Kulala CLI terminated by ${result.signal ?? "an unknown signal"}`);
    }
    process.exitCode = result.status;
}

function reportFatalError(error) {
    console.error(`zed-http: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
}

if (require.main === module) {
    main(process.argv.slice(2)).catch(reportFatalError);
}

module.exports = {
    CLI_PACKAGE,
    CLI_VERSION,
    CORE_VERSION,
    CORE_ASSETS,
    JETBRAINS_ENVIRONMENT_FILES,
    cacheHome,
    coreAsset,
    defaultJetBrainsEnvironment,
    ensureRunner,
    findJetBrainsEnvironmentFiles,
    hasJetBrainsEnvironmentConfig,
    main,
    platformKey,
    reportFatalError,
    runnerLayout,
    sha256File,
    withAutomaticEnvironmentSelection,
};
