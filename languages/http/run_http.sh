#!/bin/sh
set -eu

# The installed dev extension is a symlink to the checkout. Resolve it before
# looking for the local build, without depending on the task shell's PATH.
extension_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
installed_root=$(CDPATH= cd -L -- "$(dirname -- "$0")/../.." && pwd -L)
work_root=$(CDPATH= cd -L -- "$installed_root/../.." && pwd -P)/work/http
workspace_root=$(pwd -P)
if [ -n "${ZED_HTTP_LSP:-}" ]; then
    adapter=$ZED_HTTP_LSP
else
    adapter=
    for record in "$work_root/active-servers/"*.path; do
        [ -f "$record" ] || continue
        if ! { IFS= read -r recorded_root; IFS= read -r recorded_adapter; } < "$record"; then
            continue
        fi
        recorded_root=$(CDPATH= cd -P -- "$recorded_root" 2>/dev/null && pwd -P) || continue
        [ "$recorded_root" = "$workspace_root" ] || continue
        case $recorded_adapter in
            /*) candidate=$recorded_adapter ;;
            *) candidate=$work_root/$recorded_adapter ;;
        esac
        [ -x "$candidate" ] || continue
        adapter=$candidate
        break
    done
    if [ -z "$adapter" ] && [ -f "$extension_root/Cargo.toml" ]; then
        adapter=$extension_root/target/debug/zed-http-lsp
    fi
    if [ -z "$adapter" ]; then
        case "$(uname -s):$(uname -m)" in
            Darwin:arm64) target=aarch64-apple-darwin ;;
            Darwin:x86_64) target=x86_64-apple-darwin ;;
            Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
            Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
            *) echo "zed-http: set ZED_HTTP_LSP to the native adapter path for this platform" >&2; exit 1 ;;
        esac
        version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$extension_root/extension.toml")
        adapter=$work_root/zed-http-lsp-$target-$version/zed-http-lsp
        if [ ! -x "$adapter" ]; then
            newest=$(for candidate in "$work_root/zed-http-lsp-$target-"*/zed-http-lsp; do
                [ -x "$candidate" ] || continue
                suffix=${candidate#"$work_root"/zed-http-lsp-$target-}
                suffix=${suffix%/zed-http-lsp}
                case $suffix in
                    (""|*.*.*.*|.*|*.|*[!0-9.]*) continue ;;
                esac
                printf '%s\n' "$suffix"
            done | sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1)
            if [ -n "$newest" ]; then
                adapter=$work_root/zed-http-lsp-$target-$newest/zed-http-lsp
            fi
        fi
    fi
fi
if [ ! -x "$adapter" ]; then
    echo "zed-http: adapter not found at $adapter; open an HTTP file to install it, or build the dev adapter with cargo build --package zed-http-lsp" >&2
    exit 1
fi
exec "$adapter" "$@"
