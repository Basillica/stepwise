#!/usr/bin/env bash
# Starts a real stepwise server + builder + UI proxy, then drives the UI in jsdom.
#   needs: node, `npm install` here (jsdom), and the binaries:
#   cargo build -p stepwise -p stepwise-builder -p stepwise-ui
#   env:   BUILDER_JCO / BUILDER_COMPONENTIZE_PY if the toolchains are not on PATH
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"; root="$here/../../.."; bin="${BIN_DIR:-$root/target/debug}"
tmp="$(mktemp -d)"; pids=()
cleanup() { for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$tmp"; }
trap cleanup EXIT

CONTROL_TOKEN=ctl-e2e-token-0123456789; BUILDER_TOKEN=builder-e2e-token-0123456789; UI_TOKEN=ui-e2e-token-0123456789
cp "$root/examples/api.yaml" "$tmp/api.yaml"
cat > "$tmp/config.yaml" <<YAML
server: { host: 127.0.0.1, port: 18082 }
control: { host: 127.0.0.1, port: 19091, token: $CONTROL_TOKEN }
openapi: { spec_path: api.yaml }
database: { enabled: true, path: data/db.sqlite }
wasm:
  enabled: true
  data_dir: data
  hook_timeout_ms: 3000
  builder: { url: "http://127.0.0.1:17071", token: $BUILDER_TOKEN, timeout_secs: 280 }
logging: { level: warn, access_log: false }
YAML

BUILDER_TOKEN=$BUILDER_TOKEN BUILDER_LISTEN=127.0.0.1:17071 BUILDER_WIT_DIR="${WIT_DIR:-$root/wit}" "$bin/stepwise-builder" >"$tmp/builder.log" 2>&1 & pids+=($!)
"$bin/stepwise" run "$tmp/config.yaml" >"$tmp/server.log" 2>&1 & pids+=($!)
STEPWISE_CONTROL_URL=http://127.0.0.1:19091 STEPWISE_CONTROL_TOKEN=$CONTROL_TOKEN UI_TOKEN=$UI_TOKEN UI_LISTEN=127.0.0.1:18083 \
  "$bin/stepwise-ui" >"$tmp/ui.log" 2>&1 & pids+=($!)

for url in http://127.0.0.1:17071/v1/health http://127.0.0.1:19091/api/v1/health http://127.0.0.1:18083/healthz; do
  for _ in $(seq 1 100); do curl -sf "$url" >/dev/null 2>&1 && break; sleep 0.2; done
  curl -sf "$url" >/dev/null || { echo "service did not start: $url"; tail -5 "$tmp"/*.log; exit 1; }
done

cd "$here"
UI_URL=http://127.0.0.1:18083 UI_TOKEN=$UI_TOKEN DATA_URL=http://127.0.0.1:18082 \
CONTROL_URL=http://127.0.0.1:19091 CONTROL_TOKEN=$CONTROL_TOKEN node run.mjs || { echo "--- server log"; tail -15 "$tmp/server.log"; echo "--- builder log"; tail -5 "$tmp/builder.log"; exit 1; }
