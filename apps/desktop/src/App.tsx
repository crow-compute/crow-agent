import { useEffect, useState } from "react";
import {
  beginDeviceAuthorization,
  completeDeviceAuthorization,
  getAgentStatus,
  getRemoteState,
  sendLocalCommand,
  sendRemoteCommand,
  type AgentStatus,
  type DeviceAuthorization,
  type RemoteState,
} from "./tauri";

const initial: AgentStatus = {
  protocol: "crow.harness.v1",
  executionBoundary: "local_device",
  daemon: "connecting",
  activeRun: null,
  deviceAuthorized: false,
};

export function App() {
  const [status, setStatus] = useState(initial);
  const [authorization, setAuthorization] = useState<DeviceAuthorization | null>(null);
  const [authorizationError, setAuthorizationError] = useState<string | null>(null);
  const [authorizationBusy, setAuthorizationBusy] = useState(false);
  const [remote, setRemote] = useState<RemoteState>({ devices: [], runs: [] });
  const [remoteBusy, setRemoteBusy] = useState("");
  const [localBusy, setLocalBusy] = useState("");

  useEffect(() => {
    const refresh = () => void getAgentStatus().then((next) => {
      setStatus(next);
      if (next.deviceAuthorized) void getRemoteState().then(setRemote).catch(() => undefined);
    });
    refresh();
    const interval = window.setInterval(refresh, 2_000);
    return () => window.clearInterval(interval);
  }, []);

  async function startAuthorization() {
    setAuthorizationBusy(true);
    setAuthorizationError(null);
    try {
      setAuthorization(await beginDeviceAuthorization("Crow desktop"));
    } catch {
      setAuthorizationError("Could not start device authorization.");
    } finally {
      setAuthorizationBusy(false);
    }
  }

  async function finishAuthorization() {
    setAuthorizationBusy(true);
    setAuthorizationError(null);
    try {
      await completeDeviceAuthorization();
      setAuthorization(null);
      setStatus(await getAgentStatus());
      setRemote(await getRemoteState());
    } catch (error) {
      setAuthorizationError(
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
    setAuthorizationError(null);
    try {
      await sendRemoteCommand(deviceId, runId, action);
      setRemote(await getRemoteState());
    } catch {
      setAuthorizationError(`Remote ${action} was not accepted.`);
    } finally {
      setRemoteBusy("");
    }
  }

  async function controlLocal(action: "pause" | "resume" | "stop") {
    setLocalBusy(action);
    setAuthorizationError(null);
    try {
      setStatus(await sendLocalCommand(action));
    } catch {
      setAuthorizationError(`Local ${action} was not accepted.`);
    } finally {
      setLocalBusy("");
    }
  }

  const activeRemoteRuns = remote.runs.filter(
    (run) => run.status === "running" || run.status === "paused",
  );

  return (
    <main className="shell">
      <header>
        <div>
          <p className="eyebrow">Crow Compute / private alpha</p>
          <h1>Agent Control</h1>
        </div>
        <span className={`status status-${status.daemon}`}>{status.daemon}</span>
      </header>

      <section className="hero">
        <div>
          <p className="label">Execution boundary</p>
          <strong>Keys and strategy stay on this device.</strong>
          <p>
            Crow verifies signed decisions, receipts, orders, and fills. The webview
            never receives trading credentials.
          </p>
        </div>
        <code>{status.protocol}</code>
      </section>

      <section className="grid">
        <article>
          <p className="label">Runtime</p>
          <h2>Local daemon</h2>
          <p>Closing minimizes this controller; background execution continues.</p>
          <div className="local-controls" role="group" aria-label="Local daemon controls">
            <button
              type="button"
              disabled={status.daemon !== "running" || Boolean(localBusy)}
              onClick={() => void controlLocal("pause")}
            >
              Pause
            </button>
            <button
              type="button"
              disabled={status.daemon !== "paused" || Boolean(localBusy)}
              onClick={() => void controlLocal("resume")}
            >
              Resume
            </button>
            <button
              type="button"
              disabled={
                (status.daemon !== "running" && status.daemon !== "paused") ||
                Boolean(localBusy)
              }
              onClick={() => void controlLocal("stop")}
            >
              Stop
            </button>
          </div>
          {authorization ? (
            <div className="authorization">
              <span>Enter this code in the browser</span>
              <strong>{authorization.userCode}</strong>
              <button type="button" disabled={authorizationBusy} onClick={finishAuthorization}>
                I approved this device
              </button>
            </div>
          ) : (
            <button
              type="button"
              disabled={authorizationBusy || status.deviceAuthorized}
              onClick={startAuthorization}
            >
              {status.deviceAuthorized ? "Device authorized" : "Authorize device"}
            </button>
          )}
          {authorizationError ? <p className="error">{authorizationError}</p> : null}
        </article>
        <article>
          <p className="label">Arena</p>
          <h2>No active run</h2>
          <p>BTC, ETH, and SOL · 15-minute decisions · isolated 1×.</p>
          <button type="button" disabled>Browse arenas</button>
        </article>
        <article>
          <p className="label">Remote host</p>
          <h2>{remote.devices.length} approved device{remote.devices.length === 1 ? "" : "s"}</h2>
          <p>Commands travel through Crow&apos;s outbound-only relay. No inbound port is opened.</p>
          <button
            type="button"
            disabled={!status.deviceAuthorized || remoteBusy === "refresh"}
            onClick={() => {
              setRemoteBusy("refresh");
              void getRemoteState()
                .then(setRemote)
                .catch(() => setAuthorizationError("Could not refresh remote devices."))
                .finally(() => setRemoteBusy(""));
            }}
          >
            Refresh devices
          </button>
        </article>
      </section>

      {activeRemoteRuns.length ? (
        <section className="remote-runs" aria-label="Active remote runs">
          <p className="label">Remote lifecycle control</p>
          {activeRemoteRuns.map((run) => {
            const device = remote.devices.find((candidate) => candidate.id === run.deviceId);
            return (
              <article key={run.id}>
                <div>
                  <strong>{device?.deviceLabel || "Approved device"}</strong>
                  <span>{run.status} · release {run.clientRelease}</span>
                </div>
                <div>
                  <button
                    type="button"
                    disabled={run.status !== "running" || Boolean(remoteBusy)}
                    onClick={() => void controlRemote(run.deviceId, run.id, "pause")}
                  >
                    Pause
                  </button>
                  <button
                    type="button"
                    disabled={run.status !== "paused" || Boolean(remoteBusy)}
                    onClick={() => void controlRemote(run.deviceId, run.id, "resume")}
                  >
                    Resume
                  </button>
                  <button
                    type="button"
                    disabled={Boolean(remoteBusy)}
                    onClick={() => void controlRemote(run.deviceId, run.id, "stop")}
                  >
                    Stop
                  </button>
                </div>
              </article>
            );
          })}
        </section>
      ) : null}
    </main>
  );
}
