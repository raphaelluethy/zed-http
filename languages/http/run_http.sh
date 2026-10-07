#!/bin/sh
set -eu

# The installed dev extension is a symlink to the checkout. Resolve it before
# looking for the local build, without depending on the task shell's PATH.
extension_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
if [ -n "${ZED_HTTP_LSP:-}" ]; then
    adapter=$ZED_HTTP_LSP
elif [ -f "$extension_root/Cargo.toml" ]; then
    adapter=$extension_root/target/debug/zed-http-lsp
else
    case "$(uname -s):$(uname -m)" in
        Darwin:arm64) target=aarch64-apple-darwin ;;
        Darwin:x86_64) target=x86_64-apple-darwin ;;
        Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
        Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
        *) echo "zed-http: set ZED_HTTP_LSP to the native adapter path for this platform" >&2; exit 1 ;;
    esac
    version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$extension_root/extension.toml")
    adapter=$extension_root/../../work/http/zed-http-lsp-$target-$version/zed-http-lsp
fi
if [ ! -x "$adapter" ]; then
    echo "zed-http: adapter not found at $adapter; open an HTTP file to install it, or build the dev adapter with cargo build --package zed-http-lsp" >&2
    exit 1
fi
exec "$adapter" "$@"
