#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: verify-tauri-updater-evidence.sh <artifact-directory> <encoded-public-key>" >&2
  exit 64
fi

artifact_directory=$1
encoded_public_key=$2
if [[ ! -d "$artifact_directory" ]]; then
  echo "updater artifact directory does not exist" >&2
  exit 66
fi
if [[ -z "$encoded_public_key" ]]; then
  echo "encoded updater public key is required" >&2
  exit 78
fi
if ! command -v minisign >/dev/null 2>&1; then
  echo "minisign is required" >&2
  exit 69
fi

temporary_directory=$(mktemp -d)
trap 'rm -rf "$temporary_directory"' EXIT

decode_base64() {
  if base64 --help 2>&1 | grep -q -- '--decode'; then
    base64 --decode
  else
    base64 -D
  fi
}

public_key_file="$temporary_directory/updater.pub"
printf '%s' "$encoded_public_key" | decode_base64 > "$public_key_file"

signature_count=0
while IFS= read -r -d '' encoded_signature; do
  artifact=${encoded_signature%.sig}
  if [[ ! -f "$artifact" ]]; then
    echo "updater signature has no matching artifact: $encoded_signature" >&2
    exit 1
  fi
  decoded_signature="$temporary_directory/signature-$signature_count.minisig"
  decode_base64 < "$encoded_signature" > "$decoded_signature"
  minisign -Vm "$artifact" -x "$decoded_signature" -p "$public_key_file" >/dev/null
  signature_count=$((signature_count + 1))
done < <(find "$artifact_directory" -type f -name '*.sig' -print0)

if [[ "$signature_count" -lt 1 ]]; then
  echo "no Tauri updater signatures were found" >&2
  exit 1
fi

printf 'verified %s Tauri updater signature(s)\n' "$signature_count"
