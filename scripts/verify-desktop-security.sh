#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
capability="$repository_root/apps/desktop/src-tauri/capabilities/default.json"
configuration="$repository_root/apps/desktop/src-tauri/tauri.conf.json"
styles="$repository_root/apps/desktop/src/styles.css"
font_directory="$repository_root/apps/desktop/src/assets/fonts"
logo="$repository_root/apps/desktop/src/assets/crow-logo.png"
desktop_source="$repository_root/apps/desktop/src/App.tsx"
desktop_rust_source="$repository_root/apps/desktop/src-tauri/src/lib.rs"

grep -Fq '"permissions": ["core:default"]' "$capability"
if grep -Eq 'shell:|fs:|http:' "$capability"; then
  echo "desktop WebView capability exposes a forbidden permission" >&2
  exit 1
fi
grep -Fq "connect-src 'none'" "$configuration"
grep -Fq '"externalBin"' "$configuration"
grep -Fq '"binaries/crow-agentd"' "$configuration"
for font in \
  ibm-plex-sans-latin.woff2 \
  ibm-plex-mono-400-latin.woff2 \
  ibm-plex-mono-500-latin.woff2 \
  ibm-plex-mono-600-latin.woff2 \
  tektur-latin.woff2; do
  test -s "$font_directory/$font"
  grep -Fq "./assets/fonts/$font" "$styles"
done
test -s "$font_directory/LICENSE-IBM-PLEX.txt"
test -s "$font_directory/LICENSE-TEKTUR.txt"
test -s "$logo"
test "$(shasum -a 256 "$logo" | awk '{print $1}')" = \
  "f6a64613eb8c8391e4401185c4d097ff70a44c4f0d32b6392ed53144685f28fc"
grep -Fq 'import crowLogo from "./assets/crow-logo.png"' "$desktop_source"
grep -Fq 'className="brand-logo"' "$desktop_source"
grep -Fq 'const CREDENTIAL_VAULT_ACCOUNT: &str = "desktop-credential-vault-v1";' \
  "$desktop_rust_source"
grep -Fq 'authorization_status: Mutex::new(Some(false))' "$desktop_rust_source"
grep -Fq '"unlock_device_credentials"' \
  "$repository_root/apps/desktop/src/tauri.ts"
if test "$(grep -c 'Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_VAULT_ACCOUNT)' \
  "$desktop_rust_source")" -ne 2; then
  echo "desktop must access exactly one OS credential-store account" >&2
  exit 1
fi
if grep 'Entry::new(CREDENTIAL_SERVICE' "$desktop_rust_source" |
  grep -Fv 'CREDENTIAL_VAULT_ACCOUNT' >/dev/null; then
  echo "desktop accesses a legacy per-secret credential-store account" >&2
  exit 1
fi
if grep -Fq 'crow-mark.png' "$desktop_source"; then
  echo "desktop references the superseded symbol-only mark" >&2
  exit 1
fi
test "$(shasum -a 256 "$font_directory/ibm-plex-sans-latin.woff2" | awk '{print $1}')" = \
  "056e4e2459f57a0033c8c9c844ff19d6e42ac8602027803d4345823bcc939818"
test "$(shasum -a 256 "$font_directory/ibm-plex-mono-400-latin.woff2" | awk '{print $1}')" = \
  "c36f509c0a8f9f85f29cb44bc8701d8a9e0b14c499e77a884f789ead7093a7ac"
test "$(shasum -a 256 "$font_directory/ibm-plex-mono-500-latin.woff2" | awk '{print $1}')" = \
  "a76f53ca6612e7b3828eec2311098675b7f9849ae4169a8bcef6302aec02a6c0"
test "$(shasum -a 256 "$font_directory/ibm-plex-mono-600-latin.woff2" | awk '{print $1}')" = \
  "ad4580d8cb4b5f627c2d18457656732f7f7b070f7837fbc380e08054157e6f6c"
test "$(shasum -a 256 "$font_directory/tektur-latin.woff2" | awk '{print $1}')" = \
  "468f3e60237cb450abf4ab64f96dab0de0aee61a0339226d35899add6a1ad2ab"
if grep -Eq 'https?://' "$styles"; then
  echo "desktop WebView loads a remote font" >&2
  exit 1
fi
if grep -RIEq 'fetch[[:space:]]*\(|XMLHttpRequest|WebSocket[[:space:]]*\(' \
  "$repository_root/apps/desktop/src"; then
  echo "desktop WebView contains a direct external-network primitive" >&2
  exit 1
fi
