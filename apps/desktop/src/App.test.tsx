import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  App,
  arenaAcceptsSetup,
  arenaLaunchFailure,
  credentialUnlockFailure,
  parseHandoffSnapshot,
} from "./App";

const harnessMock = vi.hoisted(() => ({
  authorized: false,
  daemon: "ready",
  activeRun: null as string | null,
  arenas: [] as Array<Record<string, unknown>>,
  unlockCalls: 0,
}));

vi.mock("./tauri", async () => {
  const actual = await vi.importActual<typeof import("./tauri")>("./tauri");
  return {
    ...actual,
    getAgentStatus: async () => ({
      protocol: "crow.harness.v1",
      executionBoundary: "local_device",
      daemon: harnessMock.daemon,
      activeRun: harnessMock.activeRun,
      deviceAuthorized: harnessMock.authorized,
    }),
    getPublicArenas: async () => ({ arenas: harnessMock.arenas }),
    getRemoteState: async () => ({ devices: [], runs: [] }),
    getAgentVersions: async () => ({ versions: [] }),
    unlockDeviceCredentials: async () => {
      harnessMock.unlockCalls += 1;
      return { deviceId: "device", accessExpiresAt: "2026-07-28T00:00:00Z" };
    },
  };
});

describe("Crow Agent shell", () => {
  afterEach(() => {
    harnessMock.authorized = false;
    harnessMock.daemon = "ready";
    harnessMock.activeRun = null;
    harnessMock.arenas = [];
    harnessMock.unlockCalls = 0;
  });

  it("touches the credential vault only after an explicit unlock", async () => {
    render(<App />);
    expect(harnessMock.unlockCalls).toBe(0);
    screen.getByRole("button", { name: /Unlock device/ }).click();
    await waitFor(() => expect(harnessMock.unlockCalls).toBe(1));
  });

  it("presents the branded local execution command surface", () => {
    render(<App />);
    const logo = screen.getByRole("img", { name: "Crow" });
    expect(logo).toHaveClass("brand-logo");
    expect(logo.getAttribute("src")).toMatch(/crow-logo/);
    expect(screen.queryByText("LOCAL AGENT")).not.toBeInTheDocument();
    expect(screen.getByText(/Trade from/i)).toBeInTheDocument();
    expect(screen.getByText(/Secrets never enter the WebView/)).toBeInTheDocument();
    expect(screen.getByText(/Crow receives signed structured evidence/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Unlock device/ })).toBeEnabled();
    expect(screen.getByRole("button", { name: /Authorize new/ })).toBeEnabled();
    expect(screen.getByRole("group", { name: "Local daemon controls" })).toBeInTheDocument();
    expect(screen.getByText("Safety ceiling")).toBeInTheDocument();
    expect(screen.getByText("Isolated 1×")).toBeInTheDocument();
  });

  it("navigates to the real arena catalog empty state", async () => {
    render(<App />);
    screen.getByRole("button", { name: /Paper arenas/ }).click();
    expect(await screen.findByRole("heading", { name: "PAPER ARENAS" })).toBeInTheDocument();
    expect(screen.getByText("No arena manifest is open.")).toBeInTheDocument();
  });

  it("does not present an idle paused companion as an active run", async () => {
    harnessMock.authorized = true;
    harnessMock.daemon = "paused";
    render(<App />);
    expect(await screen.findByRole("heading", { name: "Ready when you are" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Resume" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Stop" })).toBeDisabled();
  });

  it("opens the immutable agent workflow from an enrollable arena", async () => {
    harnessMock.authorized = true;
    harnessMock.arenas = [{
      id: "00000000-0000-0000-0000-000000000001",
      mode: "hyperliquid_testnet",
      manifest: {
        name: "First verified Testnet arena",
        eligible_models: ["crow-qwen3-5-27b"],
      },
      state: "enrollment",
      startsAt: "2026-07-28T00:00:00Z",
      endsAt: "2099-07-29T00:00:00Z",
      ticketsEnabled: false,
      manifestSha256: "a".repeat(64),
      signerPublicKey: "public",
      signature: "signature",
    }];
    render(<App />);
    screen.getByRole("button", { name: /Paper arenas/ }).click();
    const select = await screen.findByRole("button", { name: "Select agent" });
    select.click();
    expect(await screen.findByRole("heading", { name: "First verified Testnet arena" })).toBeInTheDocument();
    expect(screen.getByText("No compatible version yet.")).toBeInTheDocument();
    expect(screen.getByLabelText("Private strategy instructions")).toBeInTheDocument();
  });

  it("fails closed for an arena whose immutable end time passed", () => {
    const arena = {
      id: "00000000-0000-0000-0000-000000000001",
      mode: "hyperliquid_testnet",
      manifest: {},
      state: "enrollment",
      startsAt: "2026-07-28T00:00:00Z",
      endsAt: "2026-07-28T00:30:00Z",
      ticketsEnabled: false,
      manifestSha256: "a".repeat(64),
      signerPublicKey: "public",
      signature: "signature",
    };
    expect(arenaAcceptsSetup(arena, Date.parse("2026-07-28T00:29:59Z"))).toBe(true);
    expect(arenaAcceptsSetup(arena, Date.parse("2026-07-28T00:30:00Z"))).toBe(false);
  });

  it("accepts only structured fixed-point handoff snapshots", () => {
    expect(parseHandoffSnapshot("")).toBeNull();
    expect(parseHandoffSnapshot('{"equity_micro_usdc":1000000,"positions":[{"quantity_e8":-42}]}'))
      .toEqual({ equity_micro_usdc: 1000000, positions: [{ quantity_e8: -42 }] });
    expect(() => parseHandoffSnapshot('{"equity":1.25}')).toThrow("handoff_snapshot_invalid");
    expect(() => parseHandoffSnapshot("[1,2,3]")).toThrow("handoff_snapshot_invalid");
  });

  it("renders bounded launch diagnostics without leaking raw errors", () => {
    expect(arenaLaunchFailure("device_authorization_failed")).toMatch(/Reauthorize/);
    expect(arenaLaunchFailure(new Error("agent_version_invalid"))).toMatch(/encrypted agent version/);
    expect(arenaLaunchFailure("unexpected secret-shaped detail")).toBe(
      "Arena launch failed closed. No order was submitted.",
    );
  });

  it("explains that a denied credential unlock will not retry", () => {
    expect(credentialUnlockFailure("credential_store_unavailable")).toMatch(
      /No more requests will be made this session/,
    );
    expect(credentialUnlockFailure("device_authorization_not_started")).toMatch(
      /Authorize new/,
    );
    expect(credentialUnlockFailure("unexpected secret-shaped detail")).toBe(
      "The local credential vault could not be unlocked. No background retry will run.",
    );
  });
});
