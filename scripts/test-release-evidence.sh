#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: test-release-evidence.sh <evidence-directory> <minisign-secret-key-file> <artifact-directory>" >&2
  exit 64
fi

evidence_directory=$1
secret_key_file=$2
artifact_directory=$3
temporary_directory=$(mktemp -d)
trap 'rm -rf "$temporary_directory"' EXIT
public_key_file="$temporary_directory/minisign.pub"
minisign -R -s "$secret_key_file" -p "$public_key_file"

./scripts/verify-release-evidence.sh "$evidence_directory" "$public_key_file"
while IFS= read -r checksum_line; do
  expected_digest=${checksum_line%%  *}
  relative_path=${checksum_line#*  }
  artifact="$artifact_directory/$relative_path"
  signature="$evidence_directory/artifact-signatures/$relative_path.minisig"
  actual_digest=$(shasum -a 256 "$artifact" | awk '{print $1}')
  if [[ "$actual_digest" != "$expected_digest" ]]; then
    echo "release artifact digest mismatch: $relative_path" >&2
    exit 1
  fi
  minisign -Vm "$artifact" -x "$signature" -p "$public_key_file" >/dev/null
done < "$evidence_directory/SHA256SUMS"
cp "$evidence_directory/release-manifest-v1.json" "$temporary_directory/manifest.json"
printf '\n' >> "$temporary_directory/manifest.json"
cp "$evidence_directory/release-manifest-v1.json.minisig" "$temporary_directory/manifest.json.minisig"
if minisign -Vm "$temporary_directory/manifest.json" -p "$public_key_file" >/dev/null 2>&1; then
  echo "modified release manifest unexpectedly verified" >&2
  exit 1
fi
