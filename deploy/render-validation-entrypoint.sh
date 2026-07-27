#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' 'CROW_RENDER_VALIDATION_START'
cat /etc/os-release
uname -m
id
sha256sum \
  /usr/local/bin/crow-agentd \
  /usr/lib/systemd/system/crow-agentd.service

/usr/local/bin/crow-agentd soak \
  --state-directory /var/lib/crow-agent/soak \
  --report /var/lib/crow-agent/soak-report.json \
  --duration-seconds 7200 \
  --interval-seconds 900

grep -F '"status": "complete"' /var/lib/crow-agent/soak-report.json
sha256sum /var/lib/crow-agent/soak-report.json /var/lib/crow-agent/soak/journal.db
printf '%s\n' 'CROW_RENDER_VALIDATION_COMPLETE'

exec sleep infinity
