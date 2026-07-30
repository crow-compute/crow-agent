# Crow Agent

Publicly readable, source-available user-hosted trading agent and verified
paper-arena harness.

The repository is intentionally separate from the GPU provider agent. It
contains:

- `crow-agent-core` — protocol validation, fixed-point risk policy, encrypted
  journal, model gateway client, deterministic backtest, and scoring.
- `crow-agentd` — outbound-only daemon for a user-controlled Linux host.
- `crow-agent-desktop` — Tauri v2 controller for macOS and Windows.

The venue API-wallet key and raw strategy transcript remain on the user's
device. Crow receives only encrypted strategy bundles, signed structured run
events, and inference receipts required to verify arena results.

## Download the free public alpha

Crow Agent v0.1.15-alpha.20 is a free, testnet-only public alpha. Do not connect
it to live capital.

- [Release page and notes](https://github.com/crow-compute/crow-agent/releases/tag/harness-v0.1.15-alpha.20)
- [macOS universal DMG](https://github.com/crow-compute/crow-agent/releases/download/harness-v0.1.15-alpha.20/Crow.Agent_0.1.15_universal.dmg)
- [Windows x86-64 installer](https://github.com/crow-compute/crow-agent/releases/download/harness-v0.1.15-alpha.20/Crow.Agent_0.1.15_x64-setup.exe)
- [Ubuntu 22.04/24.04 x86-64 daemon](https://github.com/crow-compute/crow-agent/releases/download/harness-v0.1.15-alpha.20/crow-agent-linux-x86_64.tar.zst)
- [Signed BTC/ETH/SOL historical dataset](https://github.com/crow-compute/crow-agent/releases/tag/dataset-v1)

The desktop alpha is distributed directly, not through the Apple App Store or
Microsoft Store. It is not signed with Apple Developer ID or Microsoft
Authenticode, so macOS Gatekeeper or Windows SmartScreen may require an
explicit manual allow action.

The desktop does not read the OS credential store during startup or background
polling. Click **Unlock device** when you want to use the local credential
vault. Because these direct alpha builds are ad-hoc signed, macOS may ask once
again after an update; denying that request suppresses every later credential
request for the current app session.

Every download has a detached Minisign signature. The release also includes a
signed SHA-256 inventory, signed `ReleaseManifestV1`, public verification key,
SPDX SBOM, and Grype report. Verify the evidence with:

```bash
minisign -Vm release-manifest-v1.json \
  -x release-manifest-v1.json.minisig \
  -p crow-compute-release-v1.pub
```

On Ubuntu, extract and install the package as root. The installer creates the
non-login `crow-agent` system user and installs the correct hardened systemd
unit, but deliberately does not start the service before credentials and
`/etc/crow/agent.json` exist:

```bash
tar --zstd -xf crow-agent-linux-x86_64.tar.zst
sudo ./crow-agent-linux-x86_64/install.sh
```

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

The macOS and Windows desktop application embeds the same `crow-agentd`
binary as a background companion. Rust starts it directly from the signed app
bundle and passes a separate 32-byte IPC key through the child's stdin. The key
is generated locally, stored in the OS credential store, and never enters the
React WebView. Status and pause/resume/stop requests use bounded canonical JSON
messages authenticated with HMAC-SHA256 over a cross-platform local
socket/named pipe. Nonces persist in the credential store so replayed commands
remain invalid across controller restarts. Closing the desktop window minimizes
the controller without terminating the app or companion.

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
public device ID. Copy `deploy/agent.example.json` to `/etc/crow/agent.json`,
replace its public device, arena, agent-version, release, model, and execution
account values, install `deploy/crow-agentd.service`, and start the service.
The signed arena manifest is verified before the daemon connects to the venue.
The unit opens no inbound socket.

For a hosted-runner cutover, set `handoff_snapshot` to a root-readable,
fixed-point JSON snapshot captured after the hosted run stops and reconciles.
The first local run binds that snapshot to its backend record and signed event
chain before any resume. Existing isolated 1× positions are preserved; inherited
shorts may only be reduced by reduce-only buys and can never be increased.

The live daemon starts or reclaims one bound arena run and renews its lease
every 10 seconds. A new join stays behind its fail-closed execution gate while
venue and portfolio state reconcile, then durably records an automatic resume
and enters running without a second click. After restart, a previously running
run resumes only after reconciliation, while a user-issued pause remains
paused. It keeps a persistent BTC/ETH/SOL book stream, performs bounded REST
reconciliation after reconnect, encrypts the lease and daily risk counters in
the local journal, durably records an order dispatch before sending the exact
IOC client ID, and checks the execution gate again immediately before venue
submission. A stop preserves positions and removes only the completed run's
local lease metadata.

Ubuntu 24.04 uses host-bound `LoadCredentialEncrypted` files. Ubuntu 22.04
ships systemd 249, before encrypted systemd credentials were introduced, so
the installer selects a `LoadCredential` unit whose root-only sources live
only under `/run/crow-agent-credentials`. Provisioning must repopulate that
volatile directory after every boot; plaintext credentials are never written
to persistent configuration, arguments, logs, or environment files.

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

## Deterministic replay

The macOS, Windows, and Linux CI matrix asserts the same fixed-point historical
replay and signed Parquet package against immutable SHA-256 golden values:

- backtest result:
  `67580b3c870a429b5aee61a01007809ac3862c2c5484d6ee36f9ec14e28cd77d`
- signed dataset bytes:
  `afc8195514729823fdb3ba1d372cdcfb8c4721baa0f6d4291c7dd3ff3c07d207`

Changing either digest requires an explicit review of the arena execution or
dataset-package semantics. A platform-specific serialization or arithmetic
difference fails CI.

Production historical packages are built by the protected
`signed-historical-dataset` workflow from Hyperliquid's public mainnet info API.
The publisher accepts only hour-aligned completed windows with at most 5,000
15-minute candles per symbol, requires a complete BTC/ETH/SOL series and one
funding record per symbol per hour, and refuses delisted or incomplete
instrument metadata. Prices use micro-USDC fixed point, volumes use 1e8 units,
and funding rates use 1e12 units so the replay path never parses floating-point
values.

Each release contains deterministic zstd-compressed `candles.parquet` and
`instruments.parquet` files plus `dataset-manifest-v1.json`. The manifest binds
both file hashes, the `hyperliquid-mainnet-info-v1` source, the immutable window
and version, and the dedicated dataset signer. Its pinned public key is
`deploy/crow-dataset-release-v1.pub`. Verify an extracted package with:

```bash
cargo run --release --locked -p crow-dataset-publisher -- \
  verify \
  --dataset-directory /path/to/extracted-dataset \
  --expected-public-key-file deploy/crow-dataset-release-v1.pub
```

OpenProphet is a noncommercial project and no OpenProphet source is included.
This repository remains proprietary and all rights are reserved; making the
source publicly readable does not grant an open-source license.

## Releases

Release workflows build the macOS universal desktop, Windows x86-64 desktop,
and Linux x86-64 daemon. The attest job refuses a release with a high or
critical Grype finding and emits an SPDX SBOM, SHA-256 checksums, a
`ReleaseManifestV1`, and detached Minisign signatures. Free public-alpha
desktop installers are distributed directly without Apple Developer ID
notarization or Windows Authenticode. Users should therefore expect Gatekeeper
or SmartScreen publisher warnings and must opt in manually. App Store, Windows
Store, and general-availability distribution remain separate product decisions
and may impose native platform-signing requirements.

Desktop builds also produce Tauri updater signatures. The public updater key is
embedded in `tauri.conf.json`; its private key exists only as the protected
`TAURI_SIGNING_PRIVATE_KEY` release-environment secret. The attest job verifies
each downloaded macOS and Windows updater artifact against that public key
before signing the cross-platform release manifest.

The WebView capability remains `core:default` only: it receives no generic
shell, filesystem, or HTTP permissions. Bundled companion execution is invoked
only by trusted Rust code.

Local release evidence can be reproduced with:

```bash
MINISIGN_SECRET_KEY_PATH=/run/credentials/release-key \
RELEASE_SIGNER=crow-compute-release-v1 \
./scripts/build-release-evidence.sh release/artifacts release/evidence

./scripts/verify-release-evidence.sh \
  release/evidence \
  deploy/crow-compute-release-v1.pub
```
