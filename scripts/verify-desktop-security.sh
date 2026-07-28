#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
capability="$repository_root/apps/desktop/src-tauri/capabilities/default.json"
configuration="$repository_root/apps/desktop/src-tauri/tauri.conf.json"

grep -Fq '"permissions": ["core:default"]' "$capability"
if grep -Eq 'shell:|fs:|http:' "$capability"; then
  echo "desktop WebView capability exposes a forbidden permission" >&2
  exit 1
fi
grep -Fq "connect-src 'none'" "$configuration"
grep -Fq '"externalBin"' "$configuration"
grep -Fq '"binaries/crow-agentd"' "$configuration"
if grep -RIEq 'fetch[[:space:]]*\(|XMLHttpRequest|WebSocket[[:space:]]*\(' \
  "$repository_root/apps/desktop/src"; then
  echo "desktop WebView contains a direct external-network primitive" >&2
  exit 1
fi
