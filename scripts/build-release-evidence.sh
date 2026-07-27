#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: build-release-evidence.sh <artifact-directory> <output-directory>" >&2
  exit 64
fi

artifact_directory=$1
output_directory=$2
signing_key_path=${MINISIGN_SECRET_KEY_PATH:-}
release_signer=${RELEASE_SIGNER:-}

if [[ ! -d "$artifact_directory" ]]; then
  echo "artifact directory does not exist" >&2
  exit 66
fi
if [[ -z "$signing_key_path" || ! -f "$signing_key_path" ]]; then
  echo "MINISIGN_SECRET_KEY_PATH must identify a credential file" >&2
  exit 78
fi
if [[ -z "$release_signer" ]]; then
  echo "RELEASE_SIGNER is required" >&2
  exit 78
fi
for tool_name in git jq shasum minisign syft grype; do
  if ! command -v "$tool_name" >/dev/null 2>&1; then
    echo "required tool is unavailable: $tool_name" >&2
    exit 69
  fi
done

mkdir -p "$output_directory"
source_commit=$(git rev-parse HEAD)
source_committed_at=$(git show -s --format=%cI HEAD)
release_version=${RELEASE_VERSION:-$(git describe --tags --always)}
checksum_file="$output_directory/SHA256SUMS"
sbom_file="$output_directory/release.spdx.json"
scan_file="$output_directory/grype.json"
manifest_file="$output_directory/release-manifest-v1.json"

find "$artifact_directory" -type f -print0 \
  | sort -z \
  | while IFS= read -r -d '' artifact; do
      digest=$(shasum -a 256 "$artifact" | awk '{print $1}')
      relative_path=${artifact#"$artifact_directory"/}
      printf '%s  %s\n' "$digest" "$relative_path"
    done > "$checksum_file"

syft "dir:$artifact_directory" -o "spdx-json=$sbom_file"
grype "sbom:$sbom_file" -o json > "$scan_file"
if jq -e '[.matches[] | select(.vulnerability.severity == "High" or .vulnerability.severity == "Critical")] | length > 0' "$scan_file" >/dev/null; then
  echo "release contains unresolved high or critical vulnerabilities" >&2
  exit 1
fi

sbom_digest=$(shasum -a 256 "$sbom_file" | awk '{print $1}')
scan_digest=$(shasum -a 256 "$scan_file" | awk '{print $1}')
ui_digest=null
if [[ -d apps/desktop/dist ]] && find apps/desktop/dist -type f -print -quit | grep -q .; then
  ui_digest=$(find apps/desktop/dist -type f -print0 \
    | sort -z \
    | xargs -0 shasum -a 256 \
    | shasum -a 256 \
    | awk '{print $1}')
  ui_digest="\"$ui_digest\""
fi
targets_json=$(jq -R -s '
  split("\n")
  | map(select(length > 0))
  | map(
      capture("^(?<digest>[0-9a-f]{64})  (?<path>.+)$")
      | {
          os: (
            if (.path | ascii_downcase | contains("macos")) then "macos"
            elif (.path | ascii_downcase | contains("windows")) then "windows"
            else "linux"
            end
          ),
          arch: (
            if (.path | ascii_downcase | contains("universal")) then "universal"
            else "x86_64"
            end
          ),
          artifact_sha256: .digest
        }
    )
' "$checksum_file")

jq -n \
  --arg protocol "crow.harness.v1" \
  --arg version "$release_version" \
  --arg source_commit "$source_commit" \
  --arg source_committed_at "$source_committed_at" \
  --argjson targets "$targets_json" \
  --argjson ui_sha256 "$ui_digest" \
  --arg sbom_sha256 "$sbom_digest" \
  --arg vulnerability_evidence_sha256 "$scan_digest" \
  --arg signer "$release_signer" \
  '{
    protocol: $protocol,
    version: $version,
    source_commit: $source_commit,
    source_committed_at: $source_committed_at,
    targets: $targets,
    ui_sha256: $ui_sha256,
    sbom_sha256: $sbom_sha256,
    vulnerability_report_sha256: $vulnerability_evidence_sha256,
    compatible_protocols: [$protocol],
    signer: $signer
  }' > "$manifest_file"

minisign -S -s "$signing_key_path" -m "$manifest_file"
minisign -S -s "$signing_key_path" -m "$checksum_file"

signature_directory="$output_directory/artifact-signatures"
while IFS= read -r checksum_line; do
  relative_path=${checksum_line#*  }
  signature_file="$signature_directory/$relative_path.minisig"
  mkdir -p "$(dirname "$signature_file")"
  minisign -S -s "$signing_key_path" \
    -m "$artifact_directory/$relative_path" \
    -x "$signature_file"
done < "$checksum_file"
