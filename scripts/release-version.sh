#!/usr/bin/env bash
set -euo pipefail

for tool_name in cargo jq; do
  if ! command -v "$tool_name" >/dev/null 2>&1; then
    echo "required tool is unavailable: $tool_name" >&2
    exit 69
  fi
done

release_version=${RELEASE_VERSION:-}
if [[ -z "$release_version" ]]; then
  release_version=$(cargo metadata --locked --no-deps --format-version 1 \
    | jq -r '.packages[] | select(.name == "crow-agentd") | .version')
fi
if ! [[ "$release_version" =~ ^v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z.-]+))?$ ]]; then
  echo "RELEASE_VERSION must be a semantic version accepted by crow.harness.v1" >&2
  exit 78
fi

printf '%s\n' "$release_version"
