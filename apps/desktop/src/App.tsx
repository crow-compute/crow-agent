import { useEffect, useMemo, useState } from "react";
import crowLogo from "./assets/crow-logo.png";
import {
  beginDeviceAuthorization,
  completeDeviceAuthorization,
  createAgentVersion,
  getAgentVersions,
  getAgentStatus,
  getLocalRunJournal,
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
  type LocalRunEvent,
  type LocalRunJournal,
  type LocalRunSummary,
  type PublicArena,
  type RemoteState,
} from "./tauri";

type View = "overview" | "arenas" | "runs" | "devices";
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

const studioActivityEventTypes = new Set([
  "proposal",
  "policy_outcome",
  "order_submitted",
  "venue_acknowledgement",
  "fill",
  "cycle_failed",
  "cycle_missed",
]);

function isAgentActivity(event: LocalRunEvent) {
  if (!studioActivityEventTypes.has(event.eventType)) return false;
  const source = journalObject(event.details)?.source;
  if (source === "session_reconciliation") return false;
  if (["policy_outcome", "order_submitted", "venue_acknowledgement", "fill"].includes(event.eventType)) {
    return Boolean(event.cycleId);
  }
  return true;
}

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

function eventName(value: string) {
  return value.replaceAll("_", " ").toUpperCase();
}

function detailLabel(value: string) {
  return value
    .replaceAll("_", " ")
    .replace(/\[(\d+)\]/g, " $1")
    .toUpperCase();
}

function detailValue(key: string, value: unknown) {
  if (typeof value === "boolean") return value ? "YES" : "NO";
  if (typeof value !== "number") return value === null ? "—" : String(value);
  if (key.endsWith("_micro_usdc")) {
    return `${(value / 1_000_000).toLocaleString(undefined, { maximumFractionDigits: 6 })} USDC`;
  }
  if (key.endsWith("_bps")) return `${(value / 100).toLocaleString()}%`;
  if (key.endsWith("_e8")) return (value / 100_000_000).toLocaleString(undefined, { maximumFractionDigits: 8 });
  if (key.endsWith("_time_ms") || key === "time") {
    const moment = new Date(value);
    if (!Number.isNaN(moment.getTime())) return moment.toLocaleString();
  }
  return value.toLocaleString();
}

export function flattenJournalDetails(
  value: unknown,
  prefix = "",
  depth = 0,
): Array<[string, string]> {
  if (depth > 4) return [];
  if (value === null || typeof value !== "object") {
    return prefix ? [[detailLabel(prefix), detailValue(prefix, value)]] : [];
  }
  const entries = Array.isArray(value)
    ? value.map((item, index) => [`${prefix}[${index + 1}]`, item] as const)
    : Object.entries(value).map(([key, item]) => [prefix ? `${prefix}.${key}` : key, item] as const);
  return entries
    .flatMap(([key, item]) => flattenJournalDetails(item, key, depth + 1))
    .slice(0, 30);
}

function journalObject(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

export type JournalDecision = {
  sequence: number;
  cycleId: string | null;
  action: "hold" | "order";
  status: string;
  summary: string;
  noTradeReason: string | null;
  proposal: unknown;
  receipted: boolean;
};

export function journalDecisions(events: LocalRunEvent[]): JournalDecision[] {
  return events
    .filter((event) => ["proposal", "cycle_failed", "cycle_missed"].includes(event.eventType))
    .map((event) => {
      const details = journalObject(event.details);
      if (event.eventType === "cycle_failed" || event.eventType === "cycle_missed") {
        const failed = event.eventType === "cycle_failed";
        const reason = typeof details?.reason === "string" ? details.reason : null;
        return {
          sequence: event.sequence,
          cycleId: event.cycleId,
          action: "hold",
          status: failed ? "INFERENCE FAILED" : "MISSED CYCLE",
          summary: failed
            ? "The cycle ended before a valid receipt-bound model decision was available."
            : "The scheduled decision window passed without a completed model decision.",
          noTradeReason: reason
            ? `No order was permitted because the cycle ended with ${reason.replaceAll("_", " ")}.`
            : "No order was permitted because the cycle did not complete.",
          proposal: null,
          receipted: event.receipted,
        } satisfies JournalDecision;
      }
      const legacyProposal = details?.symbol ? details : null;
      const proposal = details?.proposal ?? legacyProposal;
      const action = details?.action === "order" || proposal ? "order" : "hold";
      const related = events.filter((candidate) => candidate.cycleId === event.cycleId);
      const policyEvent = related.find((candidate) => candidate.eventType === "policy_outcome");
      const policy = journalObject(policyEvent?.details);
      const policyAllowed = typeof policy?.allowed === "boolean" ? policy.allowed : null;
      const policyReason = typeof policy?.reason === "string" ? policy.reason : null;
      const orderSubmitted = related.some((candidate) => candidate.eventType === "order_submitted");
      const fillCount = related
        .filter((candidate) => candidate.eventType === "fill")
        .reduce((total, candidate) => {
          const fills = journalObject(candidate.details)?.fills;
          return total + (Array.isArray(fills) ? fills.length : 0);
        }, 0);
      const capturedSummary = typeof details?.decision_summary === "string"
        ? details.decision_summary.trim()
        : "";

      if (action === "hold") {
        return {
          sequence: event.sequence,
          cycleId: event.cycleId,
          action,
          status: "HOLD",
          summary: capturedSummary || "The model selected HOLD. This older client cycle did not record a display-safe explanation.",
          noTradeReason: policyReason === "model_abstained"
            ? "Model abstained; there was no order for local policy or the venue to execute."
            : "No order proposal was produced, so nothing was submitted.",
          proposal: null,
          receipted: event.receipted && (policyEvent?.receipted ?? true),
        } satisfies JournalDecision;
      }

      const status = policyAllowed === false
        ? "BLOCKED BY POLICY"
        : fillCount > 0
          ? "FILLED"
          : orderSubmitted
            ? "SUBMITTED / NO FILL"
            : policyAllowed === true
              ? "POLICY APPROVED"
              : "ORDER PROPOSED";
      const noTradeReason = policyAllowed === false
        ? `Local policy rejected the proposal${policyReason ? `: ${policyReason}` : "."}`
        : fillCount > 0
          ? null
          : orderSubmitted
          ? "The IOC order reached the venue, but no fill is recorded."
          : !orderSubmitted
            ? "No venue submission is recorded for this proposal."
            : null;
      return {
        sequence: event.sequence,
        cycleId: event.cycleId,
        action,
        status,
        summary: capturedSummary || "This older client cycle recorded the proposal without a display-safe model explanation.",
        noTradeReason,
        proposal,
        receipted: event.receipted && (policyEvent?.receipted ?? true),
      } satisfies JournalDecision;
    })
    .reverse();
}

type StudioPosition = {
  symbol: string;
  quantityE8: number;
  notionalMicroUsdc: number;
  entryPriceMicroUsdc: number | null;
  unrealizedPnlMicroUsdc: number;
  leverage: number;
};

type StudioPortfolio = {
  equityMicroUsdc: number | null;
  positions: StudioPosition[];
};

function integerField(record: Record<string, unknown> | null, key: string) {
  const value = record?.[key];
  return typeof value === "number" && Number.isSafeInteger(value) ? value : null;
}

function formatMicroUsdc(value: number | null, signed = false) {
  if (value === null) return "—";
  const amount = value / 1_000_000;
  const prefix = signed && amount > 0 ? "+" : "";
  return `${prefix}${amount.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 6,
  })} USDC`;
}

function formatFixed(value: number | null, scale: number, maximumFractionDigits = 8) {
  if (value === null) return "—";
  return (value / scale).toLocaleString(undefined, { maximumFractionDigits });
}

function latestStudioPortfolio(events: LocalRunEvent[]): StudioPortfolio {
  const snapshot = [...events]
    .reverse()
    .find((event) => event.eventType === "portfolio_snapshot");
  const details = journalObject(snapshot?.details);
  const rawPositions = journalObject(details?.positions);
  const positions = Object.entries(rawPositions ?? {}).flatMap(([symbol, value]) => {
    const position = journalObject(value);
    const quantityE8 = integerField(position, "quantity_e8");
    const notionalMicroUsdc = integerField(position, "notional_micro_usdc");
    const unrealizedPnlMicroUsdc = integerField(position, "unrealized_pnl_micro_usdc");
    if (quantityE8 === null || notionalMicroUsdc === null || unrealizedPnlMicroUsdc === null) return [];
    return [{
      symbol: typeof position?.symbol === "string" ? position.symbol : symbol,
      quantityE8,
      notionalMicroUsdc,
      entryPriceMicroUsdc: integerField(position, "entry_price_micro_usdc"),
      unrealizedPnlMicroUsdc,
      leverage: integerField(position, "leverage") ?? 1,
    }];
  });
  return {
    equityMicroUsdc: integerField(details, "equity_micro_usdc"),
    positions,
  };
}

function activityAction(event: LocalRunEvent) {
  if (event.eventType === "proposal") {
    const details = journalObject(event.details);
    return details?.action === "order" || details?.proposal ? "ORDER" : "HOLD";
  }
  if (event.eventType === "policy_outcome") {
    return journalObject(event.details)?.allowed === false ? "BLOCKED" : "POLICY";
  }
  if (event.eventType === "fill") return "FILL";
  if (event.eventType === "cycle_failed") return "FAILED";
  if (event.eventType === "cycle_missed") return "MISSED";
  return eventName(event.eventType);
}

function activityTitle(event: LocalRunEvent, decisions: JournalDecision[]) {
  if (event.eventType === "proposal") {
    const decision = decisions.find((candidate) => candidate.sequence === event.sequence);
    return decision ? `Decision cycle: ${decision.action}` : "Decision cycle";
  }
  if (event.eventType === "policy_outcome") {
    const details = journalObject(event.details);
    const reason = typeof details?.reason === "string" ? details.reason.replaceAll("_", " ") : "";
    return details?.allowed === false
      ? `Policy rejected${reason ? `: ${reason}` : ""}`
      : reason === "model abstained"
        ? "Policy recorded model abstention"
        : "Policy approved";
  }
  if (event.eventType === "fill") {
    const fills = journalObject(event.details)?.fills;
    const first = Array.isArray(fills) ? journalObject(fills[0]) : null;
    return typeof first?.coin === "string" ? `${first.coin} fill` : "Venue fill";
  }
  if (event.eventType === "cycle_failed") return "Decision cycle failed safely";
  if (event.eventType === "cycle_missed") return "Scheduled decision was missed";
  return eventName(event.eventType);
}

function safeActivityDetails(event: LocalRunEvent) {
  const blocked = /(PROMPT|TRANSCRIPT|STRATEGY|CREDENTIAL|SIGNATURE|HASH|PRIVATE|SECRET|TOKEN|KEY)/;
  return flattenJournalDetails(event.details)
    .filter(([label]) => !blocked.test(label))
    .slice(0, 16);
}

export function arenaAcceptsSetup(arena: PublicArena, now = Date.now()) {
  const endsAt = new Date(arena.endsAt).getTime();
  return ["enrollment", "running"].includes(arena.state)
    && Number.isFinite(endsAt)
    && now < endsAt;
}

export type DecisionCountdown = {
  tone: "active" | "paused" | "ended" | "stopped" | "unavailable";
  label: string;
  value: string;
  boundaryAt: string | null;
  detail: string;
};

function formatCountdown(milliseconds: number) {
  const totalSeconds = Math.max(0, Math.ceil(milliseconds / 1_000));
  const hours = Math.floor(totalSeconds / 3_600);
  const minutes = Math.floor((totalSeconds % 3_600) / 60);
  const seconds = totalSeconds % 60;
  return hours
    ? `${hours}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`
    : `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}

export function nextDecisionCountdown(
  run: LocalRunSummary,
  now = Date.now(),
): DecisionCountdown {
  const startsAt = run.arenaStartsAt ? Date.parse(run.arenaStartsAt) : Number.NaN;
  const endsAt = run.arenaEndsAt ? Date.parse(run.arenaEndsAt) : Number.NaN;
  const intervalSeconds = run.decisionIntervalSeconds;
  if (
    !Number.isFinite(startsAt)
    || !Number.isFinite(endsAt)
    || startsAt >= endsAt
    || typeof intervalSeconds !== "number"
    || !Number.isSafeInteger(intervalSeconds)
    || intervalSeconds <= 0
  ) {
    return {
      tone: "unavailable",
      label: "SCHEDULE UNAVAILABLE",
      value: "—",
      boundaryAt: null,
      detail: "The signed local arena schedule could not be verified.",
    };
  }
  if (now >= endsAt) {
    return {
      tone: "ended",
      label: "ARENA ENDED",
      value: "00:00",
      boundaryAt: null,
      detail: `Ended ${formatMoment(run.arenaEndsAt)}`,
    };
  }
  if (run.state === "stopped") {
    return {
      tone: "stopped",
      label: "RUN STOPPED",
      value: "—",
      boundaryAt: null,
      detail: "No further decisions will execute.",
    };
  }

  const intervalMilliseconds = intervalSeconds * 1_000;
  const nextAt = now < startsAt
    ? startsAt
    : startsAt + (Math.floor((now - startsAt) / intervalMilliseconds) + 1)
      * intervalMilliseconds;
  if (nextAt >= endsAt) {
    return {
      tone: "ended",
      label: "DECISION WINDOWS COMPLETE",
      value: "00:00",
      boundaryAt: null,
      detail: `Arena ends ${formatMoment(run.arenaEndsAt)}`,
    };
  }

  const beforeStart = now < startsAt;
  const paused = run.state === "paused";
  return {
    tone: paused ? "paused" : "active",
    label: paused
      ? beforeStart
        ? "PAUSED — RESUME BEFORE START"
        : "PAUSED — RESUME BEFORE NEXT WINDOW"
      : beforeStart
        ? "ARENA STARTS IN"
        : "NEXT DECISION",
    value: formatCountdown(nextAt - now),
    boundaryAt: new Date(nextAt).toISOString(),
    detail: `${paused ? "Scheduled window" : "Scheduled for"} ${formatMoment(new Date(nextAt).toISOString())}`,
  };
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
      return "The signed local companion exited before reconciliation. No order was submitted.";
    case "hyperliquid_api_wallet_unavailable":
      return "The Hyperliquid execution account or local API wallet is unavailable.";
    case "hyperliquid_account_state_unavailable":
      return "Crow could not verify this Hyperliquid Testnet account's abstraction mode and collateral. Confirm the master account address and retry.";
    case "hyperliquid_testnet_collateral_required":
      return "This Hyperliquid Testnet account has no available trading collateral. Unified-account USDC is supported; no Spot-to-Perps transfer is required.";
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
  const [journal, setJournal] = useState<LocalRunJournal>({
    runs: [],
    selectedRunId: null,
    events: [],
  });
  const [journalBusy, setJournalBusy] = useState(false);
  const [journalNotice, setJournalNotice] = useState<string | null>(null);
  const [clockNow, setClockNow] = useState(() => Date.now());

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

  useEffect(() => {
    if (view !== "runs" || !status.deviceAuthorized) return;
    let active = true;
    const refreshJournal = async () => {
      try {
        const next = await getLocalRunJournal(journal.selectedRunId ?? status.activeRun);
        if (!active) return;
        setJournal(next);
        setJournalNotice(null);
      } catch {
        if (active) setJournalNotice("The encrypted local run journal could not be read.");
      } finally {
        if (active) setJournalBusy(false);
      }
    };
    setJournalBusy(true);
    void refreshJournal();
    const interval = window.setInterval(refreshJournal, 2_000);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, [view, status.deviceAuthorized, status.activeRun, journal.selectedRunId]);

  useEffect(() => {
    if (!status.activeRun) return;
    setJournal((current) => current.selectedRunId === status.activeRun
      ? current
      : { ...current, selectedRunId: status.activeRun });
  }, [status.activeRun]);

  useEffect(() => {
    if (view !== "runs") return;
    setClockNow(Date.now());
    const interval = window.setInterval(() => setClockNow(Date.now()), 1_000);
    return () => window.clearInterval(interval);
  }, [view]);

  async function selectJournalRun(runId: string) {
    setJournalBusy(true);
    setJournalNotice(null);
    try {
      setJournal(await getLocalRunJournal(runId));
    } catch {
      setJournalNotice("The selected local run could not be read.");
    } finally {
      setJournalBusy(false);
    }
  }

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
      const next = await startLocalArena(
        selectedArena.id,
        version.id,
        executionAccount,
        parsedHandoff,
      );
      setStatus(next);
      setSelectedArena(null);
      setView("overview");
      setNotice("Local arena reconciled and running. The next scheduled decision will execute automatically.");
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
  const selectedRun = journal.runs.find((run) => run.runId === journal.selectedRunId) ?? null;
  const decisionCountdown = useMemo(
    () => selectedRun ? nextDecisionCountdown(selectedRun, clockNow) : null,
    [selectedRun, clockNow],
  );
  const decisionJournal = useMemo(() => journalDecisions(journal.events), [journal.events]);
  const latestDecision = decisionJournal[0] ?? null;
  const studioPortfolio = useMemo(() => latestStudioPortfolio(journal.events), [journal.events]);
  const studioActivity = useMemo(
    () => [...journal.events]
      .filter(isAgentActivity)
      .reverse(),
    [journal.events],
  );
  const studioGrossExposure = studioPortfolio.positions.reduce(
    (total, position) => total + Math.abs(position.notionalMicroUsdc),
    0,
  );
  const studioMarginUsed = studioPortfolio.positions.reduce(
    (total, position) => total + Math.round(Math.abs(position.notionalMicroUsdc) / Math.max(1, position.leverage)),
    0,
  );
  const studioUnrealizedPnl = studioPortfolio.positions.reduce(
    (total, position) => total + position.unrealizedPnlMicroUsdc,
    0,
  );
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
            ["runs", "03", "Trades"],
            ["devices", "04", "Devices"],
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
            <span className="section-index">
              {view === "overview" ? "01" : view === "arenas" ? "02" : view === "runs" ? "03" : "04"}
            </span>
            <span>
              {view === "overview" ? "COMMAND" : view === "arenas" ? "PAPER ARENAS" : view === "runs" ? "TRADES" : "DEVICES"}
            </span>
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
                      <button type="button" disabled={!status.activeRun} onClick={() => setView("runs")}>
                        View trades
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

        {view === "runs" ? (
          <div className="view studio-trades-view">
            {!status.deviceAuthorized ? (
              <article className="authorize-card">
                <span className="empty-glyph">◇</span>
                <div>
                  <p className="meta">JOURNAL LOCKED</p>
                  <h2>Unlock this device to inspect trades</h2>
                  <p>The journal key stays in the native credential vault. Reading trade evidence never sends private payloads into the WebView.</p>
                </div>
                <button className="primary-action" type="button" disabled={authorizationBusy} onClick={unlockCredentials}>
                  <span>Unlock device</span><b>→</b>
                </button>
              </article>
            ) : journal.runs.length ? (
              selectedRun ? (
                <div className="studio-monitor">
                  <section className="studio-run-strip" aria-label="Selected paper run">
                    <label>
                      <span>Paper run</span>
                      <select
                        aria-label="Paper run"
                        value={selectedRun.runId}
                        disabled={journalBusy}
                        onChange={(event) => void selectJournalRun(event.target.value)}
                      >
                        {journal.runs.map((run) => (
                          <option value={run.runId} key={run.runId}>
                            {shortId(run.runId)} · {run.state}
                          </option>
                        ))}
                      </select>
                    </label>
                    <div className="studio-run-identity">
                      <span><i className={`state-dot state-${selectedRun.state}`} />{selectedRun.state}</span>
                      <strong>Hyperliquid Testnet</strong>
                      <small>BTC · ETH · SOL · 15-minute cycle</small>
                    </div>
                    {decisionCountdown ? (
                      <div
                        className={`studio-next-decision countdown-${decisionCountdown.tone}`}
                        role="timer"
                        aria-label={`${decisionCountdown.label}: ${decisionCountdown.value}`}
                      >
                        <span>{decisionCountdown.label}</span>
                        <strong>{decisionCountdown.value}</strong>
                        <small>{decisionCountdown.detail}</small>
                      </div>
                    ) : null}
                  </section>

                  {journalNotice ? <div className="journal-error" role="alert">{journalNotice}</div> : null}

                  <section className="studio-finance-grid" aria-label="Portfolio, treasury, and profit and loss">
                    <article className="studio-panel studio-finance-card studio-finance-primary">
                      <div className="studio-portfolio-summary">
                        <div className="studio-portfolio-balance">
                          <span className="studio-label">Portfolio</span>
                          <strong>{formatMicroUsdc(studioPortfolio.equityMicroUsdc)}</strong>
                          <small>{studioPortfolio.equityMicroUsdc === null
                            ? `${selectedRun.state} · snapshot pending`
                            : `${selectedRun.state} · ${studioPortfolio.positions.length} open position${studioPortfolio.positions.length === 1 ? "" : "s"}`}</small>
                          <dl>
                            <div><dt>Gross exposure</dt><dd>{formatMicroUsdc(studioPortfolio.equityMicroUsdc === null ? null : studioGrossExposure)}</dd></div>
                            <div><dt>Return</dt><dd>—</dd></div>
                            <div><dt>Drawdown</dt><dd>—</dd></div>
                            <div><dt>Orders / fills</dt><dd>{selectedRun.orderCount} / {selectedRun.fillCount}</dd></div>
                          </dl>
                        </div>
                        <div className="studio-portfolio-positions">
                          <div className="studio-position-heading">
                            <strong>Open positions</strong>
                            <span>{studioPortfolio.positions.length} markets</span>
                          </div>
                          {studioPortfolio.positions.length ? studioPortfolio.positions.map((position) => (
                            <article className="studio-position-row" key={position.symbol}>
                              <div>
                                <strong>{position.symbol}</strong>
                                <small>{position.quantityE8 < 0 ? "Short" : "Long"} · {position.leverage}× isolated</small>
                              </div>
                              <dl>
                                <div>
                                  <dt>Size / USD</dt>
                                  <dd>
                                    <span>{formatFixed(position.quantityE8, 100_000_000)} {position.symbol}</span>
                                    <small>{formatMicroUsdc(Math.abs(position.notionalMicroUsdc))} notional</small>
                                  </dd>
                                </div>
                                <div><dt>Avg entry</dt><dd>{formatMicroUsdc(position.entryPriceMicroUsdc)}</dd></div>
                                <div>
                                  <dt>Unrealized P&amp;L</dt>
                                  <dd className={position.unrealizedPnlMicroUsdc >= 0 ? "positive" : "negative"}>
                                    {formatMicroUsdc(position.unrealizedPnlMicroUsdc, true)}
                                  </dd>
                                </div>
                                <div><dt>Liquidation price</dt><dd>—</dd></div>
                              </dl>
                            </article>
                          )) : (
                            <div className="studio-position-empty">
                              {studioPortfolio.equityMicroUsdc === null ? "Portfolio snapshot pending." : "No open positions."}
                            </div>
                          )}
                        </div>
                      </div>
                    </article>

                    <article className="studio-panel studio-finance-card">
                      <span className="studio-label">Treasury</span>
                      <strong>—</strong>
                      <small>Available collateral</small>
                      <div className="studio-margin-bar">
                        <i style={{
                          width: studioPortfolio.equityMicroUsdc && studioPortfolio.equityMicroUsdc > 0
                            ? `${Math.min(100, (studioMarginUsed / studioPortfolio.equityMicroUsdc) * 100)}%`
                            : "0%",
                        }} />
                      </div>
                      <dl>
                        <div><dt>Margin used</dt><dd>{formatMicroUsdc(studioPortfolio.equityMicroUsdc === null ? null : studioMarginUsed)}</dd></div>
                        <div><dt>Reserve floor</dt><dd>10%</dd></div>
                        <div>
                          <dt>Utilization</dt>
                          <dd>{studioPortfolio.equityMicroUsdc && studioPortfolio.equityMicroUsdc > 0
                            ? `${((studioMarginUsed / studioPortfolio.equityMicroUsdc) * 100).toLocaleString(undefined, { maximumFractionDigits: 2 })}%`
                            : "—"}</dd>
                        </div>
                      </dl>
                    </article>

                    <article className="studio-panel studio-finance-card">
                      <span className="studio-label">P&amp;L</span>
                      <strong className={studioPortfolio.equityMicroUsdc === null ? "" : studioUnrealizedPnl >= 0 ? "positive" : "negative"}>
                        {formatMicroUsdc(studioPortfolio.equityMicroUsdc === null ? null : studioUnrealizedPnl, true)}
                      </strong>
                      <small>Open-position P&amp;L</small>
                      <dl>
                        <div><dt>Unrealized</dt><dd>{formatMicroUsdc(studioPortfolio.equityMicroUsdc === null ? null : studioUnrealizedPnl, true)}</dd></div>
                        <div><dt>Realized</dt><dd>—</dd></div>
                        <div><dt>Funding / fees</dt><dd>—</dd></div>
                      </dl>
                    </article>
                  </section>

                  <section className="studio-panel studio-latest-decision" aria-label="Latest decision">
                    {latestDecision ? (
                      <>
                        <div className="studio-decision-main">
                          <div className="studio-decision-heading">
                            <span className="studio-label">Latest decision</span>
                            <span className={`studio-action action-${latestDecision.action}`}>{latestDecision.status}</span>
                          </div>
                          <h2>Decision cycle: {latestDecision.action}</h2>
                          <p>{latestDecision.summary}</p>
                          {latestDecision.noTradeReason ? (
                            <div className="studio-decision-reason">
                              <span>Why no trade</span>
                              <strong>{latestDecision.noTradeReason}</strong>
                            </div>
                          ) : null}
                        </div>
                        <dl className="studio-decision-stats">
                          <div><dt>Action</dt><dd>{latestDecision.action}</dd></div>
                          <div><dt>Result</dt><dd>{latestDecision.status}</dd></div>
                          <div><dt>Cycle</dt><dd>{latestDecision.cycleId ? shortId(latestDecision.cycleId) : "—"}</dd></div>
                          <div><dt>Next cycle</dt><dd>{decisionCountdown?.boundaryAt ? formatMoment(decisionCountdown.boundaryAt) : "—"}</dd></div>
                        </dl>
                      </>
                    ) : (
                      <div className="studio-decision-main">
                        <span className="studio-label">Latest decision</span>
                        <h2>Waiting for the first cycle</h2>
                      </div>
                    )}
                  </section>

                  <section className="studio-panel studio-activity" aria-label="Activity log">
                    <header>
                      <div>
                        <span className="studio-label">Agent decisions · orders · fills</span>
                        <h2>Activity log</h2>
                      </div>
                      <span>{journalBusy ? "Updating…" : "Live"}</span>
                    </header>
                    <div className="studio-activity-list">
                      {studioActivity.length ? studioActivity.map((event) => {
                        const details = safeActivityDetails(event);
                        const action = activityAction(event);
                        return (
                          <details className="studio-activity-entry" key={`${event.sequence}:${event.eventType}`}>
                            <summary>
                              <time>{formatMoment(event.occurredAt)}</time>
                              <span className={`studio-action action-${action.toLowerCase()}`}>{action}</span>
                              <strong>{activityTitle(event, decisionJournal)}</strong>
                              <span className="studio-activity-toggle">Details</span>
                            </summary>
                            <div className="studio-activity-body">
                              {details.length ? (
                                <dl>
                                  {details.map(([label, value], index) => (
                                    <div key={`${label}:${index}`}><dt>{label}</dt><dd>{value}</dd></div>
                                  ))}
                                </dl>
                              ) : (
                                <p>No additional display-safe details were recorded.</p>
                              )}
                            </div>
                          </details>
                        );
                      }) : (
                        <div className="studio-activity-empty">No decisions recorded yet.</div>
                      )}
                    </div>
                  </section>
                </div>
              ) : (
                <article className="empty-state">
                  <span className="empty-glyph">00</span>
                  <div><p className="meta">RUN UNAVAILABLE</p><h2>Select a local paper run.</h2></div>
                </article>
              )
            ) : (
              <article className="empty-state">
                <span className="empty-glyph">00</span>
                <div>
                  <p className="meta">NO PAPER RUN</p>
                  <h2>No local run yet.</h2>
                  <p>Join a paper arena to start monitoring decisions and trades.</p>
                </div>
              </article>
            )}
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
              <span><b>03</b> RUNNING</span>
            </div>
            <p className="meta">LOCAL ARENA PROVISIONING</p>
            <h2 id="arena-setup-title">{arenaName(selectedArena)}</h2>
            <p className="setup-intro">
              Strategy plaintext and the venue signing key stay inside this machine. After Crow and Hyperliquid reconciliation succeeds, the run starts automatically.
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
                  <p>Enroll one wallet entry, verify the signed arena and encrypted version, bind any explicit handoff snapshot, start the local companion, reconcile positions/fills/funding, then enter running automatically. Any failed check remains fail-closed with zero orders.</p>
                </div>
                <div className="setup-actions">
                  <button type="button" disabled={arenaBusy} onClick={() => setArenaSetupStep("agent")}>Back</button>
                  <button
                    className="primary-action"
                    type="button"
                    disabled={arenaBusy || !/^0x[0-9a-fA-F]{40}$/.test(executionAccount)}
                    onClick={() => void launchArena()}
                  >
                    <span>{arenaBusy ? "Reconciling…" : "I registered it — join arena"}</span><b>→</b>
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
