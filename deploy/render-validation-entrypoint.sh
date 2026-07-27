#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' 'CROW_RENDER_VALIDATION_START'
cat /etc/os-release
uname -m
id
duration_seconds="${CROW_SOAK_DURATION_SECONDS:-1800}"
interval_seconds="${CROW_SOAK_INTERVAL_SECONDS:-900}"
case "${duration_seconds}:${interval_seconds}" in
  *[!0-9:]* | :* | *:)
    printf '%s\n' 'invalid soak duration or interval' >&2
    exit 64
    ;;
esac
if (( duration_seconds == 0 || interval_seconds == 0 || duration_seconds % interval_seconds != 0 )); then
  printf '%s\n' 'soak duration must be a positive multiple of its interval' >&2
  exit 64
fi
printf 'CROW_RENDER_VALIDATION_CONFIG duration_seconds=%s interval_seconds=%s\n' \
  "${duration_seconds}" "${interval_seconds}"
sha256sum \
  /usr/local/bin/crow-agentd \
  /usr/lib/systemd/system/crow-agentd.service

/usr/local/bin/crow-agentd soak \
  --state-directory /var/lib/crow-agent/soak \
  --report /var/lib/crow-agent/soak-report.json \
  --duration-seconds "${duration_seconds}" \
  --interval-seconds "${interval_seconds}"

grep -F '"status": "complete"' /var/lib/crow-agent/soak-report.json
sha256sum /var/lib/crow-agent/soak-report.json /var/lib/crow-agent/soak/journal.db
printf '%s\n' 'CROW_RENDER_VALIDATION_COMPLETE'

exec sleep infinity
