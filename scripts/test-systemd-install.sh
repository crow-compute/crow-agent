#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: test-systemd-install.sh <linux-package.tar.zst> <evidence.json>" >&2
  exit 64
fi
if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "test-systemd-install.sh must run as root" >&2
  exit 77
fi

package_path=$(realpath "$1")
evidence_path=$(realpath -m "$2")
for tool_name in jq openssl realpath shasum sqlite3 ss systemctl tar zstd; do
  if ! command -v "$tool_name" >/dev/null 2>&1; then
    echo "required systemd acceptance tool is unavailable: $tool_name" >&2
    exit 69
  fi
done
if [[ ! -f "$package_path" ]]; then
  echo "Linux package is unavailable" >&2
  exit 66
fi
if [[ $(ps -p 1 -o comm= | tr -d ' ') != systemd ]]; then
  echo "systemd is not PID 1" >&2
  exit 1
fi

temporary_directory=$(mktemp -d)
drop_in_directory=/etc/systemd/system/crow-agentd.service.d
report_path=/var/lib/crow-agent/systemd-acceptance-report.json
state_directory=/var/lib/crow-agent/systemd-acceptance
cleanup() {
  systemctl stop crow-agentd.service >/dev/null 2>&1 || true
  rm -rf "$temporary_directory"
}
trap cleanup EXIT

tar --zstd -xf "$package_path" -C "$temporary_directory"
package_root="$temporary_directory/crow-agent-linux-x86_64"
"$package_root/install.sh"
systemd-analyze verify /etc/systemd/system/crow-agentd.service

umask 077
systemd_major=$(systemd --version | awk 'NR == 1 { print $2 }')
credential_transport=systemd_load_credential_encrypted
credential_source_directory=/etc/credstore.encrypted
if (( systemd_major < 250 )); then
  credential_transport=systemd_load_credential_volatile
  credential_source_directory=/run/crow-agent-credentials
  install -d -m 0700 -o root -g crow-agent "$credential_source_directory"
elif ! command -v systemd-creds >/dev/null 2>&1; then
  echo "systemd-creds is unavailable on a host that supports encrypted credentials" >&2
  exit 69
fi
while IFS=: read -r credential_name credential_file; do
  if (( systemd_major < 250 )); then
    openssl rand 32 > "$credential_source_directory/$credential_file"
    chown root:crow-agent "$credential_source_directory/$credential_file"
    chmod 0600 "$credential_source_directory/$credential_file"
  else
    openssl rand 32 \
      | systemd-creds encrypt --name="$credential_name" - \
        "$credential_source_directory/$credential_file"
  fi
done <<'CREDENTIALS'
device-signing-seed:crow-agent-device-seed
device-encryption-secret:crow-agent-device-encryption
journal-key:crow-agent-journal-key
hyperliquid-api-wallet-key:crow-agent-hyperliquid-wallet
CREDENTIALS

install -d -m 0755 "$drop_in_directory"
cat > "$drop_in_directory/acceptance.conf" <<'UNIT'
[Service]
ExecStart=
ExecStart=/usr/local/bin/crow-agentd soak --state-directory /var/lib/crow-agent/systemd-acceptance --report /var/lib/crow-agent/systemd-acceptance-report.json --duration-seconds 12 --interval-seconds 6
ExecStartPre=/usr/bin/test -s %d/device-signing-seed
ExecStartPre=/usr/bin/test -s %d/device-encryption-secret
ExecStartPre=/usr/bin/test -s %d/journal-key
ExecStartPre=/usr/bin/test -s %d/hyperliquid-api-wallet-key
UNIT
systemctl daemon-reload
systemctl reset-failed crow-agentd.service
rm -f "$report_path"
rm -rf "$state_directory"

systemctl start crow-agentd.service
for _ in $(seq 1 100); do
  if [[ -f "$report_path" ]] \
    && jq -e '.status == "running" and .cycles_completed == 1' "$report_path" >/dev/null; then
    break
  fi
  sleep 0.1
done
if [[ ! -f "$report_path" ]] \
  || ! jq -e '.status == "running" and .cycles_completed == 1' "$report_path" >/dev/null; then
  systemctl status crow-agentd.service --no-pager >&2 || true
  echo "daemon did not persist its first systemd checkpoint" >&2
  exit 1
fi

first_pid=$(systemctl show crow-agentd.service --property=MainPID --value)
first_started_at=$(jq -r .started_at "$report_path")
if [[ -z "$first_pid" || "$first_pid" == 0 ]]; then
  echo "daemon has no first systemd process" >&2
  exit 1
fi
if [[ $(ps -p "$first_pid" -o user= | tr -d ' ') != crow-agent ]]; then
  echo "daemon is not running as the non-root crow-agent user" >&2
  exit 1
fi
if ss -H -lntup | grep -F "pid=$first_pid," >/dev/null; then
  echo "daemon unexpectedly opened an inbound listener" >&2
  exit 1
fi

systemctl restart crow-agentd.service
second_pid=$(systemctl show crow-agentd.service --property=MainPID --value)
if [[ -z "$second_pid" || "$second_pid" == 0 || "$second_pid" == "$first_pid" ]]; then
  echo "systemd restart did not replace the daemon process" >&2
  exit 1
fi
if [[ $(ps -p "$second_pid" -o user= | tr -d ' ') != crow-agent ]]; then
  echo "restarted daemon is not running as the non-root crow-agent user" >&2
  exit 1
fi
if [[ $(jq -r .started_at "$report_path") != "$first_started_at" ]]; then
  echo "daemon restart replaced rather than resumed the checkpoint" >&2
  exit 1
fi
if ss -H -lntup | grep -F "pid=$second_pid," >/dev/null; then
  echo "restarted daemon unexpectedly opened an inbound listener" >&2
  exit 1
fi

for _ in $(seq 1 200); do
  if jq -e '.status == "complete" and .cycles_completed == 2' "$report_path" >/dev/null; then
    break
  fi
  sleep 0.1
done
if ! jq -e '
  .status == "complete" and
  .duration_seconds == 12 and
  .interval_seconds == 6 and
  .cycles_completed == 2 and
  .events_appended == 2 and
  .journal_reopens == 2 and
  .duplicate_events_rejected == 2 and
  .sequence_gaps_rejected == 2 and
  .remote_controls_applied == 6 and
  .encrypted_recoveries == 4 and
  .plaintext_leak_scans == 2
' "$report_path" >/dev/null; then
  systemctl status crow-agentd.service --no-pager >&2 || true
  echo "restarted daemon did not complete with balanced counters" >&2
  exit 1
fi

for _ in $(seq 1 50); do
  if [[ $(systemctl is-active crow-agentd.service || true) == inactive ]]; then
    break
  fi
  sleep 0.1
done
if [[ $(systemctl is-active crow-agentd.service || true) != inactive ]]; then
  echo "daemon did not stop after the accepted run" >&2
  exit 1
fi

journal_path="$state_directory/journal.db"
journal_integrity=$(sqlite3 "$journal_path" 'PRAGMA integrity_check;')
event_count=$(sqlite3 "$journal_path" \
  "SELECT count(*) FROM run_events WHERE run_id='00000000-0000-0000-0000-000000000002';")
first_previous=$(sqlite3 "$journal_path" \
  "SELECT previous_event_sha256 FROM run_events WHERE run_id='00000000-0000-0000-0000-000000000002' AND sequence=1;")
first_hash=$(sqlite3 "$journal_path" \
  "SELECT event_sha256 FROM run_events WHERE run_id='00000000-0000-0000-0000-000000000002' AND sequence=1;")
second_previous=$(sqlite3 "$journal_path" \
  "SELECT previous_event_sha256 FROM run_events WHERE run_id='00000000-0000-0000-0000-000000000002' AND sequence=2;")
second_hash=$(sqlite3 "$journal_path" \
  "SELECT event_sha256 FROM run_events WHERE run_id='00000000-0000-0000-0000-000000000002' AND sequence=2;")
if [[ "$journal_integrity" != ok \
  || "$event_count" != 2 \
  || "$first_previous" != "$(printf '0%.0s' {1..64})" \
  || "$second_previous" != "$first_hash" \
  || "$second_hash" != "$(jq -r .last_event_sha256 "$report_path")" ]]; then
  echo "journal integrity or hash-chain verification failed" >&2
  exit 1
fi

report_sha256=$(shasum -a 256 "$report_path" | awk '{print $1}')
journal_sha256=$(shasum -a 256 "$journal_path" | awk '{print $1}')
sleep 1
if [[ "$report_sha256" != "$(shasum -a 256 "$report_path" | awk '{print $1}')" \
  || "$journal_sha256" != "$(shasum -a 256 "$journal_path" | awk '{print $1}')" ]]; then
  echo "post-stop journal or report mutation detected" >&2
  exit 1
fi

credential_directives=$(systemctl cat crow-agentd.service \
  | grep -Ec '^LoadCredential(Encrypted)?=')
credential_files=$(find "$credential_source_directory" -maxdepth 1 -type f | wc -l | tr -d ' ')
if [[ "$credential_directives" != 4 || "$credential_files" != 4 ]]; then
  echo "systemd credential configuration is incomplete" >&2
  exit 1
fi

mkdir -p "$(dirname "$evidence_path")"
source /etc/os-release
jq -n \
  --arg protocol "crow.harness.v1" \
  --arg os "$PRETTY_NAME" \
  --arg arch "$(uname -m)" \
  --arg systemd_version "$(systemd --version | head -n 1)" \
  --arg package_sha256 "$(shasum -a 256 "$package_path" | awk '{print $1}')" \
  --arg binary_sha256 "$(shasum -a 256 /usr/local/bin/crow-agentd | awk '{print $1}')" \
  --arg unit_sha256 "$(shasum -a 256 /etc/systemd/system/crow-agentd.service | awk '{print $1}')" \
  --arg credential_transport "$credential_transport" \
  --arg report_sha256 "$report_sha256" \
  --arg journal_sha256 "$journal_sha256" \
  --arg first_pid "$first_pid" \
  --arg second_pid "$second_pid" \
  --arg completed_at "$(date --utc +%Y-%m-%dT%H:%M:%SZ)" \
  --slurpfile report "$report_path" \
  '{
    protocol: $protocol,
    operating_system: $os,
    architecture: $arch,
    pid1: "systemd",
    systemd_version: $systemd_version,
    service_user: "crow-agent",
    credential_transport: $credential_transport,
    credential_files: 4,
    persistent_plaintext_credentials: 0,
    package_sha256: $package_sha256,
    binary_sha256: $binary_sha256,
    systemd_unit_sha256: $unit_sha256,
    report_sha256: $report_sha256,
    journal_sha256: $journal_sha256,
    first_pid: ($first_pid | tonumber),
    restarted_pid: ($second_pid | tonumber),
    systemd_restart_reconciled: true,
    journal_integrity: "ok",
    event_hash_chain_valid: true,
    no_inbound_listener: true,
    no_post_stop_action: true,
    report: $report[0],
    completed_at: $completed_at
  }' > "$evidence_path"
chmod 0644 "$evidence_path"
