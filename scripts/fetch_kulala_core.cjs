const childProcess = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

const VERSION = "0.36.0";
const RELEASE_BASE = `https://github.com/mistweaverco/kulala-core/releases/download/v${VERSION}`;
const TARGETS = {
    "aarch64-apple-darwin": {
        asset: "kulala-core-darwin-arm64",
        sha256: "929f627b5e86dfff838c61b22a175a3cbc79637329f4ba268b94d181149073e4",
    },
    "x86_64-apple-darwin": {
        asset: "kulala-core-darwin-x86_64",
        sha256: "c12b0d4a5df1413ebe9b29535dc97d79585c4078c2091cf28b3c5efcc7d1c15b",
    },
    "aarch64-unknown-linux-gnu": {
        asset: "kulala-core-linux-aarch64",
        sha256: "7275c3a165d03e3a8001fd6215a9f0e0d8473312afcafc216a5e1c4d336cf28a",
    },
    "x86_64-unknown-linux-gnu": {
        asset: "kulala-core-linux-x86_64",
        sha256: "64034a012568a6b73fdc5fbc8d9665c9aa954dfa4a7c15fd46a52e49fbc19ed8",
    },
    "x86_64-pc-windows-msvc": {
        asset: "kulala-core-windows-x86_64.exe",
        sha256: "551883796d20bac1575e36d7b1b5e0e7fe28729b296fddb075d42c54aa4ec8df",
    },
};

function argument(name) {
    const index = process.argv.indexOf(name);
    if (index < 0) return undefined;
    const value = process.argv[index + 1];
    if (!value || value.startsWith("--")) throw new Error(`${name} requires a value`);
    return value;
}

function hostTarget() {
    const key = `${process.platform}-${process.arch}`;
    const targets = {
        "darwin-arm64": "aarch64-apple-darwin",
        "darwin-x64": "x86_64-apple-darwin",
        "linux-arm64": "aarch64-unknown-linux-gnu",
        "linux-x64": "x86_64-unknown-linux-gnu",
        "win32-x64": "x86_64-pc-windows-msvc",
    };
    const target = targets[key];
    if (!target) throw new Error(`Kulala Core ${VERSION} has no supported asset for ${key}`);
    return target;
}

function sha256(content) {
    return crypto.createHash("sha256").update(content).digest("hex");
}

function verifiedInstallation(destination, provenancePath, expectedSourceHash) {
    if (!fs.existsSync(destination) || !fs.existsSync(provenancePath)) return false;
    try {
        const provenance = JSON.parse(fs.readFileSync(provenancePath, "utf8"));
        return provenance.version === VERSION
            && provenance.sourceSha256 === expectedSourceHash
            && provenance.installedSha256 === sha256(fs.readFileSync(destination));
    } catch {
        return false;
    }
}

async function main() {
    const target = argument("--target") ?? hostTarget();
    const release = TARGETS[target];
    if (!release) {
        throw new Error(
            `Kulala Core ${VERSION} is not pinned for ${target}; supported targets: ${Object.keys(TARGETS).join(", ")}`,
        );
    }
    const destination = path.resolve(
        argument("--output")
            ?? path.join(
                __dirname,
                "..",
                "target",
                "kulala-core",
                target.endsWith("windows-msvc") ? "kulala-core.exe" : "kulala-core",
            ),
    );
    const provenancePath = `${destination}.provenance.json`;
    if (verifiedInstallation(destination, provenancePath, release.sha256)) {
        console.log(destination);
        return;
    }

    const url = `${RELEASE_BASE}/${release.asset}`;
    const response = await fetch(url, { redirect: "follow" });
    if (!response.ok) throw new Error(`failed to download ${url}: HTTP ${response.status}`);
    const content = Buffer.from(await response.arrayBuffer());
    const actual = sha256(content);
    if (actual !== release.sha256) {
        throw new Error(
            `refusing Kulala Core ${VERSION} for ${target}: expected SHA-256 ${release.sha256}, received ${actual}`,
        );
    }

    fs.mkdirSync(path.dirname(destination), { recursive: true, mode: 0o700 });
    const temporary = `${destination}.download-${process.pid}`;
    fs.writeFileSync(temporary, content, { mode: 0o700 });
    fs.rmSync(destination, { force: true });
    fs.renameSync(temporary, destination);
    if (process.platform !== "win32") fs.chmodSync(destination, 0o700);

    if (target.endsWith("apple-darwin")) {
        if (process.platform !== "darwin") {
            throw new Error(`macOS Kulala Core assets must be prepared on macOS, not ${process.platform}`);
        }
        const signed = childProcess.spawnSync(
            "/usr/bin/codesign",
            ["--force", "--sign", "-", destination],
            { encoding: "utf8" },
        );
        if (signed.status !== 0) {
            throw new Error(`failed to ad-hoc sign Kulala Core: ${signed.stderr || signed.stdout}`);
        }
    }

    fs.writeFileSync(
        provenancePath,
        `${JSON.stringify({
            version: VERSION,
            target,
            asset: release.asset,
            sourceSha256: release.sha256,
            installedSha256: sha256(fs.readFileSync(destination)),
        }, null, 2)}\n`,
    );
    console.log(destination);
}

main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
});
