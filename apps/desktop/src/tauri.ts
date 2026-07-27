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

export async function sendRemoteCommand(
  targetDeviceId: string,
  runId: string,
  action: "pause" | "resume" | "stop",
): Promise<{ commandId: string; action: string; accepted: boolean }> {
  return invoke("send_remote_command", { targetDeviceId, runId, action });
}
