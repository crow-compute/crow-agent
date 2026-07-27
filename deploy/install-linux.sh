#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "install-linux.sh must run as root" >&2
  exit 77
fi

package_directory=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ ! -x "$package_directory/usr/local/bin/crow-agentd" ]]; then
  echo "signed package does not contain crow-agentd" >&2
  exit 66
fi

if ! getent group crow-agent >/dev/null; then
  groupadd --system crow-agent
fi
if ! getent passwd crow-agent >/dev/null; then
  useradd --system --gid crow-agent --home-dir /var/lib/crow-agent \
    --shell /usr/sbin/nologin crow-agent
fi

install -d -m 0700 -o crow-agent -g crow-agent /var/lib/crow-agent
install -d -m 0750 -o root -g crow-agent /etc/crow
install -d -m 0700 -o root -g root /etc/credstore.encrypted
install -m 0755 "$package_directory/usr/local/bin/crow-agentd" /usr/local/bin/crow-agentd
install -m 0644 "$package_directory/lib/systemd/system/crow-agentd.service" \
  /etc/systemd/system/crow-agentd.service
systemctl daemon-reload

echo "Crow Agent installed but not started."
echo "Create encrypted credentials, authorize the device, and write /etc/crow/agent.json first."
