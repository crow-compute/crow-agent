#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: verify-release-evidence.sh <evidence-directory> <minisign-public-key-file>" >&2
  exit 64
fi

evidence_directory=$1
public_key_file=$2
minisign -Vm "$evidence_directory/release-manifest-v1.json" -p "$public_key_file"
minisign -Vm "$evidence_directory/SHA256SUMS" -p "$public_key_file"
jq -e '
  .protocol == "crow.harness.v1" and
  (.source_commit | length == 40) and
  (.protocol_versions == ["crow.harness.v1"]) and
  (.signer | length > 0)
' "$evidence_directory/release-manifest-v1.json" >/dev/null
