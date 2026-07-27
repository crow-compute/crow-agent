#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: package-linux.sh <crow-agentd-binary> <output.tar.zst>" >&2
  exit 64
fi

binary=$1
output=$2
if [[ ! -x "$binary" ]]; then
  echo "crow-agentd binary is unavailable" >&2
  exit 66
fi
for tool_name in git install tar zstd; do
  if ! command -v "$tool_name" >/dev/null 2>&1; then
    echo "required tool is unavailable: $tool_name" >&2
    exit 69
  fi
done

temporary_directory=$(mktemp -d)
trap 'rm -rf "$temporary_directory"' EXIT
package_root="$temporary_directory/crow-agent-linux-x86_64"
install -d "$package_root/usr/local/bin" "$package_root/lib/systemd/system" \
  "$package_root/share/doc/crow-agent"
install -m 0755 "$binary" "$package_root/usr/local/bin/crow-agentd"
install -m 0644 deploy/crow-agentd.service \
  "$package_root/lib/systemd/system/crow-agentd.service"
install -m 0644 deploy/crow-agentd-jammy.service \
  "$package_root/lib/systemd/system/crow-agentd-jammy.service"
install -m 0755 deploy/install-linux.sh "$package_root/install.sh"
install -m 0644 README.md "$package_root/share/doc/crow-agent/README.md"

mkdir -p "$(dirname "$output")"
source_epoch=$(git show -s --format=%ct HEAD)
tar --sort=name --mtime="@$source_epoch" --owner=0 --group=0 --numeric-owner \
  -C "$temporary_directory" -I 'zstd -19 -T0' -cf "$output" \
  crow-agent-linux-x86_64
