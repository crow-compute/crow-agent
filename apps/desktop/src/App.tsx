import { useEffect, useState } from "react";
import { getAgentStatus, type AgentStatus } from "./tauri";

const initial: AgentStatus = {
  protocol: "crow.harness.v1",
  executionBoundary: "local_device",
  daemon: "connecting",
  activeRun: null,
};

export function App() {
  const [status, setStatus] = useState(initial);

  useEffect(() => {
    void getAgentStatus().then(setStatus);
  }, []);

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
          <p>Background execution continues independently of this window.</p>
          <button type="button" disabled>Authorize device</button>
        </article>
        <article>
          <p className="label">Arena</p>
          <h2>No active run</h2>
          <p>BTC, ETH, and SOL · 15-minute decisions · isolated 1×.</p>
          <button type="button" disabled>Browse arenas</button>
        </article>
        <article>
          <p className="label">Remote host</p>
          <h2>Outbound only</h2>
          <p>Pair a Linux daemon without opening an inbound network port.</p>
          <button type="button" disabled>Add server</button>
        </article>
      </section>
    </main>
  );
}

