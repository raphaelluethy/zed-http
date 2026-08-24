import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptsDirectory = path.dirname(fileURLToPath(import.meta.url));
const repositoryRoot = path.dirname(scriptsDirectory);
const runnerPath = path.join(scriptsDirectory, "kulala_runner_bootstrap.cjs");
const tasksPath = path.join(repositoryRoot, "languages", "http", "tasks.json");

function embeddedRunnerArgument() {
    const source = fs.readFileSync(runnerPath, "utf8");
    const entrypoint = `${source}\nmain(process.argv.slice(1)).catch(reportFatalError);\n`;
    const encoded = Buffer.from(entrypoint).toString("base64");
    return `"eval(Buffer.from('${encoded}','base64').toString('utf8'))"`;
}

function generatedTasks() {
    const runner = embeddedRunnerArgument();
    return [
        {
            label: "Run HTTP request at cursor",
            command: "node",
            args: ["-e", runner, "run", "$ZED_FILE", "--line", "$ZED_ROW"],
            tags: ["http-request"],
            reveal: "always",
            save: "current",
            shell: { program: "/bin/sh" },
        },
        {
            label: "Run all HTTP requests in file",
            command: "node",
            args: ["-e", runner, "run", "$ZED_FILE"],
            reveal: "always",
            save: "current",
            shell: { program: "/bin/sh" },
        },
    ];
}

const output = `${JSON.stringify(generatedTasks(), null, 4)}\n`;
const mode = process.argv[2];

if (mode === "--write") {
    fs.writeFileSync(tasksPath, output);
} else if (mode === "--check") {
    const current = fs.readFileSync(tasksPath, "utf8");
    if (current !== output) {
        console.error("languages/http/tasks.json is stale; run node scripts/generate_http_tasks.mjs --write");
        process.exitCode = 1;
    }
} else {
    process.stdout.write(output);
}
