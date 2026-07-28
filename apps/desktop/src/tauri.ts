import { invoke } from "@tauri-apps/api/core";

export type AgentStatus = {
  protocol: "crow.harness.v1";
  executionBoundary: "local_device";
  daemon: "stopped" | "connecting" | "ready" | "running" | "paused";
  activeRun: string | null;
  deviceAuthorized: boolean;
};

export type DeviceAuthorization = {
  userCode: string;
  verificationUri: string;
  expiresAt: string;
};

export type AuthorizedDevice = {
  deviceId: string;
  accessExpiresAt: string;
};

export type RemoteDevice = {
  id: string;
  deviceLabel: string;
  platform: string;
  state: string;
  lastSeenAt: string | null;
};

export type RemoteRun = {
  id: string;
  arenaId: string;
  deviceId: string;
  status: "pending" | "running" | "paused" | "stopped" | "completed" | "disqualified";
  clientRelease: string;
  startedAt: string | null;
};

export type RemoteState = {
  devices: RemoteDevice[];
  runs: RemoteRun[];
};

export type PublicArena = {
  id: string;
  mode: string;
  manifest: Record<string, unknown>;
  state: string;
  startsAt: string;
  endsAt: string;
  ticketsEnabled: boolean;
  manifestSha256: string;
  signerPublicKey: string;
  signature: string;
};

export type PublicArenaState = {
  arenas: PublicArena[];
};

export type AgentVersionSummary = {
  id: string;
  agentId: string;
  version: number;
  modelId: string;
  configurationSha256: string;
  createdAt: string;
};

export type AgentVersionState = {
  versions: AgentVersionSummary[];
};

export type HyperliquidWalletSetup = {
  address: string;
  approvalUrl: string;
};

const fallbackStatus: AgentStatus = {
  protocol: "crow.harness.v1",
  executionBoundary: "local_device",
  daemon: "stopped",
  activeRun: null,
  deviceAuthorized: false,
};

export async function getAgentStatus(): Promise<AgentStatus> {
  if (!("__TAURI_INTERNALS__" in window)) return fallbackStatus;
  return invoke<AgentStatus>("get_agent_status");
}

export async function sendLocalCommand(
  action: "pause" | "resume" | "stop",
): Promise<AgentStatus> {
  return invoke<AgentStatus>("send_local_command", { action });
}

export async function beginDeviceAuthorization(
  deviceLabel: string,
): Promise<DeviceAuthorization> {
  return invoke<DeviceAuthorization>("begin_device_authorization", { deviceLabel });
}

export async function completeDeviceAuthorization(): Promise<AuthorizedDevice> {
  return invoke<AuthorizedDevice>("complete_device_authorization");
}

export async function getRemoteState(): Promise<RemoteState> {
  if (!("__TAURI_INTERNALS__" in window)) return { devices: [], runs: [] };
  return invoke<RemoteState>("get_remote_state");
}

export async function getPublicArenas(): Promise<PublicArenaState> {
  if (!("__TAURI_INTERNALS__" in window)) return { arenas: [] };
  return invoke<PublicArenaState>("get_public_arenas");
}

export async function getAgentVersions(): Promise<AgentVersionState> {
  if (!("__TAURI_INTERNALS__" in window)) return { versions: [] };
  return invoke<AgentVersionState>("get_agent_versions");
}

export async function createAgentVersion(
  name: string,
  modelId: string,
  systemInstructions: string,
): Promise<AgentVersionSummary> {
  return invoke<AgentVersionSummary>("create_agent_version", {
    name,
    modelId,
    systemInstructions,
  });
}

export async function prepareHyperliquidWallet(): Promise<HyperliquidWalletSetup> {
  return invoke<HyperliquidWalletSetup>("prepare_hyperliquid_wallet");
}

export async function enrollArena(
  arenaId: string,
  agentVersionId: string,
  modelId: string,
): Promise<void> {
  return invoke("enroll_arena", { arenaId, agentVersionId, modelId });
}

export async function startLocalArena(
  arenaId: string,
  agentVersionId: string,
  executionAccount: string,
  handoffSnapshot: Record<string, unknown> | null = null,
): Promise<AgentStatus> {
  return invoke<AgentStatus>("start_local_arena", {
    arenaId,
    agentVersionId,
    executionAccount,
    handoffSnapshot,
  });
}

export async function sendRemoteCommand(
  targetDeviceId: string,
  runId: string,
  action: "pause" | "resume" | "stop",
): Promise<{ commandId: string; action: string; accepted: boolean }> {
  return invoke("send_remote_command", { targetDeviceId, runId, action });
}
