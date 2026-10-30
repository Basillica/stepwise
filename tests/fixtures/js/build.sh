#!/usr/bin/env bash
# Builds the test guest components. Needs node (jco) — and optionally Python (componentize-py).
#   ./build.sh [outdir]      default outdir: ../../../target/fixtures
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="${1:-$here/../../../target/fixtures}"; mkdir -p "$out"
wit="$here/../../../wit"
jco="${JCO:-$(command -v jco || echo "$here/node_modules/.bin/jco")}"
# QuickJS backend, no WASI imports at all: smallest artifact, zero ambient capabilities.
"$jco" componentize "$here/kitchen-sink.ts" --wit "$wit" --world-name hook \
  --backend qjs --backend-qjs-disable-async --backend-qjs-stub-wasi -o "$out/kitchen-sink-qjs.wasm"
# Variant B differs only in `normalize` (lower-case instead of Title Case) so tests can tell
# versions apart after an alias moves.
tmp="$(mktemp -d)"; sed 's/c.toUpperCase()/c.toLowerCase()/' "$here/kitchen-sink.ts" > "$tmp/kitchen-sink-b.ts"
"$jco" componentize "$tmp/kitchen-sink-b.ts" --wit "$wit" --world-name hook \
  --backend qjs --backend-qjs-disable-async --backend-qjs-stub-wasi -o "$out/kitchen-sink-b-qjs.wasm"
rm -rf "$tmp"
if [ "${WITH_STARLINGMONKEY:-0}" = 1 ]; then
  # The host provides no wasi:http, so build without it (uploads that import it are rejected).
  "$jco" componentize "$here/kitchen-sink.ts" --wit "$wit" --world-name hook \
    --disable http --disable fetch-event -o "$out/kitchen-sink-sm.wasm"
fi
ls -la "$out"
