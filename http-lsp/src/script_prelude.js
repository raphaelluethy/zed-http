// The IntelliJ HTTP client script API, evaluated before each script. Inputs arrive as JSON in
// `__zedHttpInput`; `__zedHttpFinish()` returns the collected effects as JSON.
(() => {
    "use strict";

    const input = JSON.parse(__zedHttpInput);
    const MAX_LOGS = 1000;
    const MAX_TESTS = 1000;
    const MAX_MESSAGE = 64 * 1024;
    // Thrown by client.exit() to unwind the current script.
    const EXIT = Object.freeze({ zedHttpExit: true });

    const state = {
        store: Object.assign(Object.create(null), input.globals),
        globals: [],
        variables: Object.assign(Object.create(null), input.requestVariables),
        changedVariables: Object.create(null),
        logs: [],
        tests: [],
        queued: [],
        exited: false,
        truncated: false,
    };

    const text = (value) => {
        if (typeof value === "string") return value;
        if (value === undefined || typeof value === "function" || typeof value === "symbol") {
            return String(value);
        }
        try {
            const json = JSON.stringify(value);
            return json === undefined ? String(value) : json;
        } catch (_) {
            return String(value);
        }
    };

    // Globals must survive the JSON round trip back to Rust.
    const plain = (value) => {
        if (value === undefined) return null;
        try {
            const json = JSON.stringify(value);
            return json === undefined ? null : JSON.parse(json);
        } catch (_) {
            return String(value);
        }
    };

    const describe = (error) =>
        error instanceof Error ? error.message || String(error) : text(error);

    const log = (level, values) => {
        if (state.logs.length >= MAX_LOGS) {
            state.truncated = true;
            return;
        }
        let message = Array.prototype.map.call(values, text).join(" ");
        if (message.length > MAX_MESSAGE) message = `${message.slice(0, MAX_MESSAGE)}…`;
        state.logs.push({ level, message });
    };

    const lookup = (map, name) => {
        const value = map[String(name)];
        return value === undefined ? null : value;
    };

    const headerValues = (headers, name) => {
        const wanted = String(name).toLowerCase();
        return headers.filter(([key]) => key.toLowerCase() === wanted).map(([, value]) => value);
    };

    const client = Object.freeze({
        global: Object.freeze({
            set(name, value) {
                name = String(name);
                value = plain(value);
                state.store[name] = value;
                state.globals.push({ op: "set", name, value });
            },
            get(name) {
                return lookup(state.store, name);
            },
            isEmpty() {
                return Object.keys(state.store).length === 0;
            },
            clear(name) {
                name = String(name);
                delete state.store[name];
                state.globals.push({ op: "clear", name });
            },
            clearAll() {
                for (const name of Object.keys(state.store)) delete state.store[name];
                state.globals.push({ op: "clearAll" });
            },
        }),
        test(name, callback) {
            state.queued.push({ name: String(name), callback });
        },
        assert(condition, message) {
            if (!condition) throw new Error(message === undefined ? "assertion failed" : text(message));
        },
        log(...values) {
            log("log", values);
        },
        exit() {
            state.exited = true;
            throw EXIT;
        },
    });

    const request = Object.freeze({
        method: input.request.method,
        url: input.request.url,
        headers: Object.freeze({
            all() {
                return input.request.headers.map(([name, value]) => ({ name, value }));
            },
            findByName(name) {
                const values = headerValues(input.request.headers, name);
                return values.length > 0 ? values[0] : null;
            },
        }),
        variables: Object.freeze({
            set(name, value) {
                name = String(name);
                value = text(value);
                state.variables[name] = value;
                state.changedVariables[name] = value;
            },
            get(name) {
                return lookup(state.variables, name);
            },
        }),
        environment: Object.freeze({
            get(name) {
                return lookup(input.environment, name);
            },
        }),
    });

    const responseApi = (raw) => {
        const [mimeType, ...parameters] = (raw.contentType || "").split(";");
        const charset = parameters
            .map((parameter) => parameter.trim())
            .find((parameter) => parameter.toLowerCase().startsWith("charset="));
        return Object.freeze({
            status: raw.status,
            body: raw.body,
            headers: Object.freeze({
                valueOf(name) {
                    const values = headerValues(raw.headers, name);
                    return values.length > 0 ? values[0] : null;
                },
                valuesOf(name) {
                    return headerValues(raw.headers, name);
                },
            }),
            contentType: Object.freeze({
                mimeType: mimeType.trim() || null,
                charset: charset ? charset.slice("charset=".length).replace(/"/g, "") : null,
            }),
        });
    };

    // Returns undefined without a match, the value for one match and an array otherwise.
    const jsonPath = (value, expression) => {
        const json = typeof value === "string" ? value : JSON.stringify(value);
        const matches = JSON.parse(__zedHttpJsonPath(json === undefined ? "null" : json, expression));
        if (matches.length === 0) return undefined;
        return matches.length === 1 ? matches[0] : matches;
    };

    const hasher = (algorithm) => {
        let data = "";
        const self = {
            updateWithText(value) {
                data += String(value);
                return self;
            },
            digest() {
                return Object.freeze({
                    toHex: () => __zedHttpDigest(algorithm, data, "hex"),
                    toBase64: () => __zedHttpDigest(algorithm, data, "base64"),
                });
            },
        };
        return self;
    };

    const random = (name) => __zedHttpDynamic(name);
    const $random = Object.freeze({
        get uuid() {
            return random("$random.uuid");
        },
        get email() {
            return random("$random.email");
        },
        integer: (from = 0, to = 1000) => Number(random(`$random.integer(${from}, ${to})`)),
        float: (from = 0, to = 1000) => Number(random(`$random.float(${from}, ${to})`)),
        alphabetic: (length = 10) => random(`$random.alphabetic(${length})`),
        alphanumeric: (length = 10) => random(`$random.alphanumeric(${length})`),
        hexadecimal: (length = 10) => random(`$random.hexadecimal(${length})`),
    });

    globalThis.client = client;
    globalThis.request = request;
    if (input.response) globalThis.response = responseApi(input.response);
    globalThis.jsonPath = jsonPath;
    globalThis.crypto = Object.freeze({
        md5: () => hasher("md5"),
        sha1: () => hasher("sha1"),
        sha256: () => hasher("sha256"),
        sha512: () => hasher("sha512"),
    });
    globalThis.$random = $random;
    globalThis.console = Object.freeze({
        log: (...values) => log("log", values),
        info: (...values) => log("info", values),
        warn: (...values) => log("warn", values),
        error: (...values) => log("error", values),
    });

    globalThis.__zedHttpFinish = () => {
        for (const test of state.queued) {
            if (state.tests.length >= MAX_TESTS) {
                state.truncated = true;
                break;
            }
            try {
                test.callback();
                state.tests.push({ name: test.name, passed: true });
            } catch (error) {
                if (error === EXIT) break;
                state.tests.push({ name: test.name, passed: false, message: describe(error) });
            }
        }
        return JSON.stringify({
            globals: state.globals,
            variables: state.changedVariables,
            logs: state.logs,
            tests: state.tests,
            exited: state.exited,
            truncated: state.truncated,
        });
    };
})();
