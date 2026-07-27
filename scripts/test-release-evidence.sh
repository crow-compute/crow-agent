#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: test-release-evidence.sh <evidence-directory> <minisign-secret-key-file>" >&2
  exit 64
fi

evidence_directory=$1
secret_key_file=$2
temporary_directory=$(mktemp -d)
trap 'rm -rf "$temporary_directory"' EXIT
public_key_file="$temporary_directory/minisign.pub"
minisign -R -s "$secret_key_file" -p "$public_key_file"

./scripts/verify-release-evidence.sh "$evidence_directory" "$public_key_file"
cp "$evidence_directory/release-manifest-v1.json" "$temporary_directory/manifest.json"
printf '\n' >> "$temporary_directory/manifest.json"
cp "$evidence_directory/release-manifest-v1.json.minisig" "$temporary_directory/manifest.json.minisig"
if minisign -Vm "$temporary_directory/manifest.json" -p "$public_key_file" >/dev/null 2>&1; then
  echo "modified release manifest unexpectedly verified" >&2
  exit 1
fi
