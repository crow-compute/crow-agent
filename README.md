# Crow Agent

Private user-hosted trading agent and verified paper-arena harness.

The repository is intentionally separate from the GPU provider agent. It
contains:

- `crow-agent-core` — protocol validation, fixed-point risk policy, encrypted
  journal, model gateway client, deterministic backtest, and scoring.
- `crow-agentd` — outbound-only daemon for a user-controlled Linux host.
- `crow-agent-desktop` — Tauri v2 controller for macOS and Windows.

The venue API-wallet key and raw strategy transcript remain on the user's
device. Crow receives only encrypted strategy bundles, signed structured run
events, and inference receipts required to verify arena results.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test

cd apps/desktop
npm ci
npm test
npm run build
```

The daemon refuses plaintext key CLI flags and reads unattended Linux secrets
only from files below systemd's `CREDENTIALS_DIRECTORY`. The initial
browser authorization writes the rotating refresh token directly into the
encrypted SQLite/WAL journal; it is never printed. Every replacement is
persisted before the next relay session. The journal key, Ed25519 seed, X25519
secret, and Hyperliquid Testnet API-wallet key are separate
`LoadCredentialEncrypted` inputs.

For a headless host, generate the three device credentials locally with
`openssl rand 32 | systemd-creds encrypt --name=<credential> - <destination>`.
Capture the 32-byte Hyperliquid Testnet API-wallet key through a hidden prompt
and pipe its decoded bytes directly to `systemd-creds`; do not place its hex
value in shell history. Run the one-time authorization command with the signing,
encryption, and journal credentials loaded:

```bash
crow-agentd authorize "Trading host" \
  --state-directory /var/lib/crow-agent
```

The command prints only the short-lived Crow URL/user code and the resulting
public device ID. Put that public ID and the outbound relay URL in
`/etc/crow/agent.json`, install `deploy/crow-agentd.service`, and start the
service. The unit opens no inbound socket.

The release candidate also includes a checkpointed 30-minute component soak:

```bash
crow-agentd soak \
  --state-directory /var/lib/crow-agent/soak \
  --report /var/lib/crow-agent/soak-report.json
```

It runs at the production 15-minute cadence, reopens the encrypted journal
every cycle, rejects duplicate and sequence-gap events, exercises local
pause/resume/stop transitions, recovers encrypted token/private state, scans
state files for plaintext leakage, and updates the JSON report after every
cycle. This component gate complements—rather than replaces—the staging soak
that uses an authorized Crow device and actual Hyperliquid Testnet account.

OpenProphet is a noncommercial project and no OpenProphet source is included.

## Releases

Release workflows build the macOS universal desktop, Windows x86-64 desktop,
and Linux x86-64 daemon. The attest job refuses a release with a high or
critical Grype finding and emits an SPDX SBOM, SHA-256 checksums, a
`ReleaseManifestV1`, and detached Minisign signatures. Code-signing and
notarization credentials are supplied only by the repository's protected
release environment.

Local release evidence can be reproduced with:

```bash
MINISIGN_SECRET_KEY_PATH=/run/credentials/release-key \
RELEASE_SIGNER=crow-compute-release-v1 \
./scripts/build-release-evidence.sh release/artifacts release/evidence

./scripts/verify-release-evidence.sh \
  release/evidence \
  deploy/crow-compute-release-v1.pub
```
