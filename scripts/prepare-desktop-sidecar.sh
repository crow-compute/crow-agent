#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
binary_directory="$repository_root/apps/desktop/src-tauri/binaries"
mkdir -p "$binary_directory"

if [[ "$target" == "universal-apple-darwin" ]]; then
  cargo build \
    --manifest-path "$repository_root/Cargo.toml" \
    --locked \
    --release \
    --target aarch64-apple-darwin \
    -p crow-agentd
  cargo build \
    --manifest-path "$repository_root/Cargo.toml" \
    --locked \
    --release \
    --target x86_64-apple-darwin \
    -p crow-agentd
  lipo -create \
    "$repository_root/target/aarch64-apple-darwin/release/crow-agentd" \
    "$repository_root/target/x86_64-apple-darwin/release/crow-agentd" \
    -output "$binary_directory/crow-agentd-universal-apple-darwin"
  chmod 0755 "$binary_directory/crow-agentd-universal-apple-darwin"
  exit 0
fi

extension=""
if [[ "$target" == *-windows-* ]]; then
  extension=".exe"
fi

cargo build \
  --manifest-path "$repository_root/Cargo.toml" \
  --locked \
  --release \
  --target "$target" \
  -p crow-agentd
cp \
  "$repository_root/target/$target/release/crow-agentd$extension" \
  "$binary_directory/crow-agentd-$target$extension"
chmod 0755 "$binary_directory/crow-agentd-$target$extension"
