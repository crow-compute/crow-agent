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
only from files below systemd's `CREDENTIALS_DIRECTORY`.

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
```
