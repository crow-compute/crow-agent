#!/usr/bin/env bash
set -euo pipefail

test "$(./scripts/release-version.sh)" = "0.1.2"
test "$(RELEASE_VERSION=0.1.0-alpha.4 ./scripts/release-version.sh)" = \
  "0.1.0-alpha.4"
if RELEASE_VERSION=291b4c2 ./scripts/release-version.sh >/dev/null 2>&1; then
  echo "a Git short SHA unexpectedly passed release-version validation" >&2
  exit 1
fi
