import { invoke } from "@tauri-apps/api/core";

export type AgentStatus = {
  protocol: "crow.harness.v1";
  executionBoundary: "local_device";
  daemon: "stopped" | "connecting" | "ready" | "running" | "paused";
  activeRun: string | null;
};

const fallbackStatus: AgentStatus = {
  protocol: "crow.harness.v1",
  executionBoundary: "local_device",
  daemon: "stopped",
  activeRun: null,
};

export async function getAgentStatus(): Promise<AgentStatus> {
  if (!("__TAURI_INTERNALS__" in window)) return fallbackStatus;
  return invoke<AgentStatus>("get_agent_status");
}

