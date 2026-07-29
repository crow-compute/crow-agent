import { useEffect, useMemo, useState } from "react";
import crowLogo from "./assets/crow-logo.png";
import {
  beginDeviceAuthorization,
  completeDeviceAuthorization,
  createAgentVersion,
  enrollArena,
  getAgentVersions,
  getAgentStatus,
  getPublicArenas,
  getRemoteState,
  prepareHyperliquidWallet,
  sendLocalCommand,
  sendRemoteCommand,
  startLocalArena,
  unlockDeviceCredentials,
  type AgentVersionSummary,
  type AgentStatus,
  type DeviceAuthorization,
  type HyperliquidWalletSetup,
  type PublicArena,
  type RemoteState,
} from "./tauri";

type View = "overview" | "arenas" | "devices";
type ArenaSetupStep = "agent" | "venue";

const initial: AgentStatus = {
  protocol: "crow.harness.v1",
  executionBoundary: "local_device",
  daemon: "connecting",
  activeRun: null,
  deviceAuthorized: false,
};

const safetyRules = [
  ["Leverage", "Isolated 1×"],
  ["Order cap", "2% equity"],
  ["Position cap", "10% equity"],
  ["Daily loss", "2% stop"],
  ["Drawdown", "10% stop"],
  ["Cadence", "15 minutes"],
];

function shortId(value: string) {
  return `${value.slice(0, 7)}…${value.slice(-5)}`;
}

function formatMoment(value: string | null) {
  if (!value) return "Never connected";
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return "Unknown";
  return parsed.toLocaleString([], {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function arenaName(arena: PublicArena) {
  const name = arena.manifest.name;
  return typeof name === "string" && name.trim() ? name : `${arena.mode.replaceAll("_", " ")} arena`;
}

function arenaModels(arena: PublicArena) {
  const models = arena.manifest.eligible_models;
  return Array.isArray(models) ? models.filter((model): model is string => typeof model === "string") : [];
}

export function arenaAcceptsSetup(arena: PublicArena, now = Date.now()) {
  const endsAt = new Date(arena.endsAt).getTime();
  return ["enrollment", "running"].includes(arena.state)
    && Number.isFinite(endsAt)
    && now < endsAt;
}

function fixedPointHandoffValue(value: unknown): boolean {
  if (value === null || typeof value === "boolean" || typeof value === "string") return true;
  if (typeof value === "number") return Number.isSafeInteger(value);
  if (Array.isArray(value)) return value.every(fixedPointHandoffValue);
  if (typeof value === "object") {
    return Object.values(value as Record<string, unknown>).every(fixedPointHandoffValue);
  }
  return false;
}

export function parseHandoffSnapshot(raw: string): Record<string, unknown> | null {
  if (!raw.trim()) return null;
  const value: unknown = JSON.parse(raw);
  if (!value || Array.isArray(value) || typeof value !== "object" || !fixedPointHandoffValue(value)) {
    throw new Error("handoff_snapshot_invalid");
  }
  return value as Record<string, unknown>;
}

export function arenaLaunchFailure(error: unknown) {
  const code = typeof error === "string" ? error : error instanceof Error ? error.message : "";
  switch (code) {
    case "handoff_snapshot_invalid":
      return "The handoff snapshot must be a structured object containing fixed-point integers only.";
    case "device_authorization_failed":
      return "The local device token could not be forked for the runner. Reauthorize this device and retry.";
    case "agent_version_invalid":
      return "The encrypted agent version could not be opened by this device or is not eligible for the arena.";
    case "arena_operation_failed":
      return "Crow rejected the enrollment or immutable arena prerequisite. No order was submitted.";
    case "local_companion_unavailable":
      return "The signed local companion did not reach a paused reconciled run. No order was submitted.";
    case "hyperliquid_api_wallet_unavailable":
      return "The Hyperliquid execution account or local API wallet is unavailable.";
    default:
      return "Arena launch failed closed. No order was submitted.";
  }
}

export function credentialUnlockFailure(error: unknown) {
  const code = typeof error === "string" ? error : error instanceof Error ? error.message : "";
  switch (code) {
    case "device_authorization_not_started":
      return "No current credential vault exists. Authorize new to create one.";
    case "credential_store_unavailable":
      return "Credential access was denied. No more requests will be made this session. Fully quit and reopen Crow Agent when you want to retry.";
    default:
      return "The local credential vault could not be unlocked. No background retry will run.";
  }
}

export function App() {
  const [view, setView] = useState<View>("overview");
  const [status, setStatus] = useState(initial);
  const [authorization, setAuthorization] = useState<DeviceAuthorization | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [authorizationBusy, setAuthorizationBusy] = useState(false);
  const [remote, setRemote] = useState<RemoteState>({ devices: [], runs: [] });
  const [arenas, setArenas] = useState<PublicArena[]>([]);
  const [remoteBusy, setRemoteBusy] = useState("");
  const [localBusy, setLocalBusy] = useState("");
  const [selectedArena, setSelectedArena] = useState<PublicArena | null>(null);
  const [arenaSetupStep, setArenaSetupStep] = useState<ArenaSetupStep>("agent");
  const [arenaLaunchNotice, setArenaLaunchNotice] = useState<string | null>(null);
  const [agentVersions, setAgentVersions] = useState<AgentVersionSummary[]>([]);
  const [selectedVersionId, setSelectedVersionId] = useState("");
  const [agentName, setAgentName] = useState("Measured momentum");
  const [agentInstructions, setAgentInstructions] = useState(
    "Trade only when current BTC, ETH, or SOL evidence is internally consistent. Prefer hold over weak conviction. Never exceed the arena policy and reduce inherited risk before adding exposure.",
  );
  const [agentModelId, setAgentModelId] = useState("");
  const [executionAccount, setExecutionAccount] = useState("");
  const [handoffSnapshot, setHandoffSnapshot] = useState("");
  const [walletSetup, setWalletSetup] = useState<HyperliquidWalletSetup | null>(null);
  const [arenaBusy, setArenaBusy] = useState(false);

  useEffect(() => {
    let active = true;
    const refresh = async () => {
      try {
        const next = await getAgentStatus();
        if (!active) return;
        setStatus(next);
        if (next.deviceAuthorized) {
          const state = await getRemoteState();
          if (active) setRemote(state);
        }
      } catch {
        if (active) setStatus((current) => ({ ...current, daemon: "stopped" }));
      }
    };
    const loadArenas = async () => {
      try {
        const next = await getPublicArenas();
        if (active) setArenas(next.arenas);
      } catch {
        if (active) setNotice("Arena catalog is temporarily unavailable.");
      }
    };
    void refresh();
    void loadArenas();
    const interval = window.setInterval(refresh, 2_000);
    const arenaInterval = window.setInterval(loadArenas, 15_000);
    return () => {
      active = false;
      window.clearInterval(interval);
      window.clearInterval(arenaInterval);
    };
  }, []);

  async function startAuthorization() {
    setAuthorizationBusy(true);
    setNotice(null);
    try {
      setAuthorization(await beginDeviceAuthorization("Crow desktop"));
    } catch {
      setNotice("Could not start device authorization.");
    } finally {
      setAuthorizationBusy(false);
    }
  }

  async function unlockCredentials() {
    setAuthorizationBusy(true);
    setNotice(null);
    try {
      await unlockDeviceCredentials();
      setStatus(await getAgentStatus());
      setRemote(await getRemoteState());
      setNotice("Local credential vault unlocked for this app session.");
    } catch (error) {
      setNotice(credentialUnlockFailure(error));
    } finally {
      setAuthorizationBusy(false);
    }
  }

  async function finishAuthorization() {
    setAuthorizationBusy(true);
    setNotice(null);
    try {
      await completeDeviceAuthorization();
      setAuthorization(null);
      setStatus(await getAgentStatus());
      setRemote(await getRemoteState());
      setNotice("Device approved. Signing and encryption keys remain local.");
    } catch (error) {
      setNotice(
        error === "device_authorization_pending"
          ? "Wallet approval is still pending."
          : "Could not complete device authorization.",
      );
    } finally {
      setAuthorizationBusy(false);
    }
  }

  async function controlRemote(
    deviceId: string,
    runId: string,
    action: "pause" | "resume" | "stop",
  ) {
    const operation = `${runId}:${action}`;
    setRemoteBusy(operation);
    setNotice(null);
    try {
      await sendRemoteCommand(deviceId, runId, action);
      setRemote(await getRemoteState());
      setNotice(`Remote ${action} accepted by Crow relay.`);
    } catch {
      setNotice(`Remote ${action} was not accepted.`);
    } finally {
      setRemoteBusy("");
    }
  }

  async function controlLocal(action: "pause" | "resume" | "stop") {
    setLocalBusy(action);
    setNotice(null);
    try {
      setStatus(await sendLocalCommand(action));
      setNotice(`Local run ${action} accepted.`);
    } catch {
      setNotice(`Local ${action} was not accepted.`);
    } finally {
      setLocalBusy("");
    }
  }

  async function openArenaSetup(arena: PublicArena) {
    setSelectedArena(arena);
    setArenaSetupStep("agent");
    setArenaLaunchNotice(null);
    setWalletSetup(null);
    setHandoffSnapshot("");
    setArenaBusy(true);
    setNotice(null);
    setArenaLaunchNotice(null);
    const models = arenaModels(arena);
    setAgentModelId(models[0] ?? "");
    try {
      const state = await getAgentVersions();
      const eligible = state.versions.filter((version) => models.includes(version.modelId));
      setAgentVersions(eligible);
      setSelectedVersionId(eligible[0]?.id ?? "");
    } catch {
      setAgentVersions([]);
      setNotice("Could not load immutable agent versions.");
    } finally {
      setArenaBusy(false);
    }
  }

  async function createVersionForArena() {
    if (!selectedArena || !agentModelId) return;
    setArenaBusy(true);
    setNotice(null);
    try {
      const version = await createAgentVersion(agentName, agentModelId, agentInstructions);
      setAgentVersions((current) => [version, ...current]);
      setSelectedVersionId(version.id);
      setNotice("Immutable strategy encrypted and wrapped to your approved devices.");
    } catch {
      setNotice("The immutable agent version could not be created.");
    } finally {
      setArenaBusy(false);
    }
  }

  async function continueToVenue() {
    if (!selectedVersionId) return;
    setArenaBusy(true);
    setNotice(null);
    try {
      setWalletSetup(await prepareHyperliquidWallet());
      setArenaSetupStep("venue");
    } catch {
      setNotice("The local Hyperliquid API wallet could not be prepared.");
    } finally {
      setArenaBusy(false);
    }
  }

  async function launchArena() {
    if (!selectedArena || !selectedVersionId || !walletSetup) return;
    const version = agentVersions.find((candidate) => candidate.id === selectedVersionId);
    if (!version) return;
    setArenaBusy(true);
    setNotice(null);
    try {
      const parsedHandoff = parseHandoffSnapshot(handoffSnapshot);
      await enrollArena(selectedArena.id, version.id, version.modelId);
      const next = await startLocalArena(
        selectedArena.id,
        version.id,
        executionAccount,
        parsedHandoff,
      );
      setStatus(next);
      setSelectedArena(null);
      setView("overview");
      setNotice("Local arena staged and reconciled in pause. Review the run, then Resume.");
    } catch (error) {
      setArenaLaunchNotice(arenaLaunchFailure(error));
    } finally {
      setArenaBusy(false);
    }
  }

  const activeRemoteRuns = useMemo(
    () => remote.runs.filter((run) => run.status === "running" || run.status === "paused"),
    [remote.runs],
  );
  const localLive = Boolean(status.activeRun)
    && (status.daemon === "running" || status.daemon === "paused");
  const readiness = [
    { label: "Device", value: status.deviceAuthorized ? "Approved" : "Unlock required", ready: status.deviceAuthorized },
    { label: "Runtime", value: status.daemon, ready: status.daemon !== "stopped" && status.daemon !== "connecting" },
    { label: "Arena", value: status.activeRun ? shortId(status.activeRun) : "No active run", ready: Boolean(status.activeRun) },
  ];

  return (
    <main className="app-shell">
      <aside className="rail">
        <div className="brand-lockup">
          <img className="brand-logo" src={crowLogo} alt="Crow" />
        </div>

        <nav aria-label="Agent navigation">
          {([
            ["overview", "01", "Command"],
            ["arenas", "02", "Paper arenas"],
            ["devices", "03", "Devices"],
          ] as const).map(([target, index, label]) => (
            <button
              type="button"
              className={view === target ? "nav-item active" : "nav-item"}
              aria-current={view === target ? "page" : undefined}
              onClick={() => setView(target)}
              key={target}
            >
              <span>{index}</span>
              {label}
            </button>
          ))}
        </nav>

        <div className="rail-boundary">
          <span className="boundary-icon" aria-hidden="true">◇</span>
          <div>
            <strong>LOCAL CUSTODY</strong>
            <small>Secrets never enter the WebView or Crow storage.</small>
          </div>
        </div>

        <p className="protocol-label">{status.protocol}<br />alpha / testnet only</p>
      </aside>

      <section className="workspace">
        <header className="topbar">
          <div>
            <span className="section-index">{view === "overview" ? "01" : view === "arenas" ? "02" : "03"}</span>
            <span>{view === "overview" ? "COMMAND" : view === "arenas" ? "PAPER ARENAS" : "DEVICES"}</span>
          </div>
          <div className="runtime-chip">
            <i className={`state-dot state-${status.daemon}`} />
            DAEMON {status.daemon}
          </div>
        </header>

        {notice ? (
          <div className="notice" role="status">
            <span>System</span>
            <p>{notice}</p>
            <button type="button" aria-label="Dismiss message" onClick={() => setNotice(null)}>×</button>
          </div>
        ) : null}

        {view === "overview" ? (
          <div className="view command-view">
            <section className="command-hero">
              <div>
                <p className="kicker"><span /> USER-HOSTED EXECUTION</p>
                <h1>TRADE FROM<br />YOUR MACHINE.<br /><em>PROVE EVERY CYCLE.</em></h1>
              </div>
              <div className="hero-note">
                <span>THE BOUNDARY</span>
                <p>Your model loop, strategy, and venue key run here. Crow receives signed structured evidence—not your private transcript.</p>
              </div>
            </section>

            <section className="readiness-grid" aria-label="Harness readiness">
              {readiness.map((item) => (
                <article className="readiness-item" key={item.label}>
                  <span>{item.label}</span>
                  <strong><i className={item.ready ? "ready" : ""} />{item.value}</strong>
                </article>
              ))}
            </section>

            <section className="console-grid">
              <article className="runtime-panel">
                <div className="panel-heading">
                  <div>
                    <p className="meta">LOCAL RUNTIME</p>
                    <h2>{localLive ? "Run in progress" : "Ready when you are"}</h2>
                  </div>
                  <span className="machine-state">{status.daemon}</span>
                </div>

                <div className="route-visual" aria-hidden="true">
                  <span className="route-node">MODEL</span>
                  <i className={localLive ? "route-line active" : "route-line"} />
                  <span className="route-node heat">POLICY</span>
                  <i className={localLive ? "route-line active delay" : "route-line"} />
                  <span className="route-node">VENUE</span>
                </div>

                <dl className="runtime-facts">
                  <div><dt>Execution</dt><dd>Local companion</dd></div>
                  <div><dt>Network</dt><dd>Outbound relay only</dd></div>
                  <div><dt>Universe</dt><dd>BTC · ETH · SOL</dd></div>
                  <div><dt>Active run</dt><dd>{status.activeRun ? shortId(status.activeRun) : "—"}</dd></div>
                </dl>

                <div className="action-row" role="group" aria-label="Local daemon controls">
                  {!status.deviceAuthorized ? (
                    <>
                      <button className="primary-action" type="button" disabled={authorizationBusy} onClick={unlockCredentials}>
                        <span>Unlock device</span><b>→</b>
                      </button>
                      <button type="button" disabled={authorizationBusy} onClick={startAuthorization}>
                        Authorize new
                      </button>
                    </>
                  ) : (
                    <>
                      <button
                        type="button"
                        disabled={!status.activeRun || status.daemon !== "running" || Boolean(localBusy)}
                        onClick={() => void controlLocal("pause")}
                      >
                        Pause
                      </button>
                      <button
                        type="button"
                        disabled={!status.activeRun || status.daemon !== "paused" || Boolean(localBusy)}
                        onClick={() => void controlLocal("resume")}
                      >
                        Resume
                      </button>
                      <button
                        className="danger-action"
                        type="button"
                        disabled={!status.activeRun || !localLive || Boolean(localBusy)}
                        onClick={() => void controlLocal("stop")}
                      >
                        Stop
                      </button>
                    </>
                  )}
                </div>
              </article>

              <article className="policy-panel">
                <div className="panel-heading">
                  <div>
                    <p className="meta">HARD POLICY</p>
                    <h2>Safety ceiling</h2>
                  </div>
                  <span className="policy-lock">LOCKED</span>
                </div>
                <p className="panel-copy">Arena rules can tighten these limits. They can never loosen them.</p>
                <div className="rules">
                  {safetyRules.map(([label, value]) => (
                    <div key={label}><span>{label}</span><strong>{value}</strong></div>
                  ))}
                </div>
              </article>
            </section>
          </div>
        ) : null}

        {view === "arenas" ? (
          <div className="view">
            <section className="view-heading">
              <div>
                <p className="kicker"><span /> VERIFIED TESTNET COMPETITION</p>
                <h1>PAPER ARENAS</h1>
              </div>
              <p>Deterministic history or actual Hyperliquid Testnet execution. Every eligible result carries a complete signed event chain.</p>
            </section>

            <div className="arena-list">
              {arenas.length ? arenas.map((arena, index) => (
                <article className="arena-card" key={arena.id}>
                  <div className="arena-number">{String(index + 1).padStart(2, "0")}</div>
                  <div className="arena-main">
                    <p className="meta">{arena.mode.replaceAll("_", " ")} / {arena.state}</p>
                    <h2>{arenaName(arena)}</h2>
                    <p>{arenaModels(arena).join(" · ") || "Eligible Crow models published in manifest"}</p>
                  </div>
                  <dl>
                    <div><dt>Starts</dt><dd>{formatMoment(arena.startsAt)}</dd></div>
                    <div><dt>Ends</dt><dd>{formatMoment(arena.endsAt)}</dd></div>
                    <div><dt>Tickets</dt><dd>{arena.ticketsEnabled ? "Enabled" : "Free"}</dd></div>
                  </dl>
                  <button
                    type="button"
                    disabled={
                      !status.deviceAuthorized
                      || !arenaAcceptsSetup(arena)
                      || Boolean(status.activeRun)
                    }
                    onClick={() => void openArenaSetup(arena)}
                  >
                    {arenaAcceptsSetup(arena)
                      ? "Select agent"
                      : ["enrollment", "running"].includes(arena.state)
                        ? "Closed"
                        : arena.state}
                  </button>
                </article>
              )) : (
                <article className="empty-state">
                  <span className="empty-glyph">00</span>
                  <div>
                    <p className="meta">CATALOG CLEAR</p>
                    <h2>No arena manifest is open.</h2>
                    <p>The first free Hyperliquid Testnet arena will appear here after its immutable schedule, models, and scoring rules are published.</p>
                  </div>
                </article>
              )}
            </div>
          </div>
        ) : null}

        {view === "devices" ? (
          <div className="view">
            <section className="view-heading">
              <div>
                <p className="kicker"><span /> OUTBOUND-ONLY CONTROL</p>
                <h1>DEVICES</h1>
              </div>
              <p>Desktop and Linux hosts establish their own Crow connection. Remote control never requires an inbound server port.</p>
            </section>

            {!status.deviceAuthorized ? (
              <article className="authorize-card">
                <span className="empty-glyph">◇</span>
                <div>
                  <p className="meta">THIS MACHINE</p>
                  <h2>Unlock or approve this device</h2>
                  <p>The vault stays closed until you click Unlock. A direct alpha update may trigger one macOS prompt; denying it suppresses every later request for this session. Authorize new starts the wallet flow.</p>
                </div>
                <div className="credential-actions">
                  <button className="primary-action" type="button" disabled={authorizationBusy} onClick={unlockCredentials}>
                    <span>Unlock device</span><b>→</b>
                  </button>
                  <button type="button" disabled={authorizationBusy} onClick={startAuthorization}>
                    Authorize new
                  </button>
                </div>
              </article>
            ) : (
              <div className="device-list">
                {remote.devices.map((device) => {
                  const runs = activeRemoteRuns.filter((run) => run.deviceId === device.id);
                  return (
                    <article className="device-card" key={device.id}>
                      <div className="device-title">
                        <i className={device.state === "active" ? "ready" : ""} />
                        <div>
                          <h2>{device.deviceLabel}</h2>
                          <p>{device.platform} · {shortId(device.id)}</p>
                        </div>
                        <span>{device.state}</span>
                      </div>
                      <p className="last-seen">Last seen {formatMoment(device.lastSeenAt)}</p>
                      {runs.map((run) => (
                        <div className="remote-run" key={run.id}>
                          <div>
                            <span>RUN {shortId(run.id)}</span>
                            <strong>{run.status} · {run.clientRelease}</strong>
                          </div>
                          <div role="group" aria-label={`Controls for ${device.deviceLabel}`}>
                            <button type="button" disabled={run.status !== "running" || Boolean(remoteBusy)} onClick={() => void controlRemote(run.deviceId, run.id, "pause")}>Pause</button>
                            <button type="button" disabled={run.status !== "paused" || Boolean(remoteBusy)} onClick={() => void controlRemote(run.deviceId, run.id, "resume")}>Resume</button>
                            <button className="danger-action" type="button" disabled={Boolean(remoteBusy)} onClick={() => void controlRemote(run.deviceId, run.id, "stop")}>Stop</button>
                          </div>
                        </div>
                      ))}
                    </article>
                  );
                })}
                {!remote.devices.length ? (
                  <article className="empty-state compact">
                    <span className="empty-glyph">00</span>
                    <div><p className="meta">INVENTORY</p><h2>No approved devices returned.</h2><p>Refresh occurs automatically every two seconds.</p></div>
                  </article>
                ) : null}
              </div>
            )}
          </div>
        ) : null}
      </section>

      {selectedArena ? (
        <div className="authorization-layer" role="dialog" aria-modal="true" aria-labelledby="arena-setup-title">
          <section className="arena-setup-card">
            <button className="dialog-close" type="button" aria-label="Close arena setup" onClick={() => setSelectedArena(null)}>×</button>
            <div className="setup-progress" aria-label="Arena setup progress">
              <span className={arenaSetupStep === "agent" ? "active" : "complete"}><b>01</b> AGENT</span>
              <i />
              <span className={arenaSetupStep === "venue" ? "active" : ""}><b>02</b> VENUE</span>
              <i />
              <span><b>03</b> PAUSED</span>
            </div>
            <p className="meta">LOCAL ARENA PROVISIONING</p>
            <h2 id="arena-setup-title">{arenaName(selectedArena)}</h2>
            <p className="setup-intro">
              Strategy plaintext and the venue signing key stay inside this machine. The run starts paused after Crow and Hyperliquid reconciliation.
            </p>
            {arenaLaunchNotice ? (
              <div className="setup-error" role="alert">
                <strong>Could not stage arena</strong>
                <span>{arenaLaunchNotice}</span>
              </div>
            ) : null}

            {arenaSetupStep === "agent" ? (
              <div className="setup-body">
                {agentVersions.length ? (
                  <label className="field-block">
                    <span>Immutable version</span>
                    <select value={selectedVersionId} onChange={(event) => setSelectedVersionId(event.target.value)}>
                      {agentVersions.map((version) => (
                        <option value={version.id} key={version.id}>
                          {version.modelId} · v{version.version} · {shortId(version.configurationSha256)}
                        </option>
                      ))}
                    </select>
                  </label>
                ) : (
                  <div className="setup-callout">
                    <strong>No compatible version yet.</strong>
                    <span>Create one locally below. Crow receives ciphertext, metadata, and integrity hashes only.</span>
                  </div>
                )}

                <div className="setup-divider"><span>NEW IMMUTABLE VERSION</span></div>

                <div className="field-grid">
                  <label className="field-block">
                    <span>Agent name</span>
                    <input value={agentName} maxLength={80} onChange={(event) => setAgentName(event.target.value)} />
                  </label>
                  <label className="field-block">
                    <span>Model</span>
                    <select value={agentModelId} onChange={(event) => setAgentModelId(event.target.value)}>
                      {arenaModels(selectedArena).map((model) => <option value={model} key={model}>{model}</option>)}
                    </select>
                  </label>
                </div>
                <label className="field-block">
                  <span>Private strategy instructions</span>
                  <textarea
                    aria-label="Private strategy instructions"
                    value={agentInstructions}
                    maxLength={8192}
                    rows={6}
                    onChange={(event) => setAgentInstructions(event.target.value)}
                  />
                  <small>{agentInstructions.length.toLocaleString()} / 8,192 · encrypted before upload</small>
                </label>
                <div className="setup-actions">
                  <button
                    type="button"
                    disabled={arenaBusy || !agentName.trim() || !agentInstructions.trim() || !agentModelId}
                    onClick={() => void createVersionForArena()}
                  >
                    Create & encrypt
                  </button>
                  <button
                    className="primary-action"
                    type="button"
                    disabled={arenaBusy || !selectedVersionId}
                    onClick={() => void continueToVenue()}
                  >
                    <span>{arenaBusy ? "Preparing…" : "Continue to venue"}</span><b>→</b>
                  </button>
                </div>
              </div>
            ) : (
              <div className="setup-body">
                <div className="venue-key-panel">
                  <span>LOCAL API WALLET ADDRESS</span>
                  <strong>{walletSetup?.address ?? "Preparing…"}</strong>
                  <p>Register this public address as an API wallet in the Hyperliquid Testnet page that just opened. The private key is already sealed in your OS credential store.</p>
                  <button
                    className="text-action"
                    type="button"
                    onClick={() => void continueToVenue()}
                  >
                    Reopen Hyperliquid Testnet ↗
                  </button>
                </div>
                <label className="field-block">
                  <span>Hyperliquid master account</span>
                  <input
                    aria-label="Hyperliquid master account"
                    value={executionAccount}
                    placeholder="0x…"
                    spellCheck={false}
                    autoComplete="off"
                    onChange={(event) => setExecutionAccount(event.target.value)}
                  />
                  <small>Used only for account queries and run binding. Never enter a private key.</small>
                </label>
                <label className="field-block">
                  <span>Position-preserving handoff snapshot (optional)</span>
                  <textarea
                    aria-label="Position-preserving handoff snapshot"
                    value={handoffSnapshot}
                    rows={5}
                    spellCheck={false}
                    placeholder='{"protocol":"crow.harness.handoff.v1","equity_micro_usdc":1000000000,"positions":[]}'
                    onChange={(event) => setHandoffSnapshot(event.target.value)}
                  />
                  <small>Required when replacing a hosted runner with open positions. Only structured fixed-point JSON is accepted; venue keys never belong here.</small>
                </label>
                <div className="launch-contract">
                  <span>ON LAUNCH</span>
                  <p>Enroll one wallet entry, verify the signed arena and encrypted version, bind any explicit handoff snapshot, start the local companion, reconcile positions/fills/funding, and remain paused. Zero orders are permitted until Resume.</p>
                </div>
                <div className="setup-actions">
                  <button type="button" disabled={arenaBusy} onClick={() => setArenaSetupStep("agent")}>Back</button>
                  <button
                    className="primary-action"
                    type="button"
                    disabled={arenaBusy || !/^0x[0-9a-fA-F]{40}$/.test(executionAccount)}
                    onClick={() => void launchArena()}
                  >
                    <span>{arenaBusy ? "Reconciling…" : "I registered it — stage paused"}</span><b>→</b>
                  </button>
                </div>
              </div>
            )}
          </section>
        </div>
      ) : null}

      {authorization ? (
        <div className="authorization-layer" role="dialog" aria-modal="true" aria-labelledby="authorization-title">
          <section className="authorization-card">
            <button className="dialog-close" type="button" aria-label="Close authorization" onClick={() => setAuthorization(null)}>×</button>
            <p className="meta">WALLET AUTHORIZATION</p>
            <h2 id="authorization-title">Approve this machine.</h2>
            <p>The browser is open. Confirm that it shows this one-time code, then sign with your Crow wallet.</p>
            <strong className="user-code">{authorization.userCode}</strong>
            <div className="authorization-steps">
              <span><b>01</b> Match the code</span>
              <span><b>02</b> Sign in browser</span>
              <span><b>03</b> Return here</span>
            </div>
            <button className="primary-action" type="button" disabled={authorizationBusy} onClick={finishAuthorization}>
              <span>{authorizationBusy ? "Checking approval…" : "I approved this device"}</span><b>→</b>
            </button>
            <small>Expires {formatMoment(authorization.expiresAt)}</small>
          </section>
        </div>
      ) : null}
    </main>
  );
}
